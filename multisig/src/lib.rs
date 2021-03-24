#![no_std]
#![allow(non_snake_case)]

mod action;
mod proxy;
mod smart_contract_call;
mod user_role;

use action::{Action, ActionFullInfo};
use proxy::*;
use smart_contract_call::*;
use transaction::*;
use user_role::UserRole;

elrond_wasm::imports!();

/// Multi-signature smart contract implementation.
/// Acts like a wallet that needs multiple signers for any action performed.
#[elrond_wasm_derive::contract(MultisigImpl)]
pub trait Multisig {
    #[init]
    fn init(
        &self,
        required_stake: BigUint,
        slash_amount: BigUint,
        quorum: usize,
        #[var_args] board: VarArgs<Address>,
    ) -> SCResult<()> {
        require!(
            !board.is_empty(),
            "board cannot be empty on init, no-one would be able to propose"
        );
        require!(quorum <= board.len(), "quorum cannot exceed board size");
        self.quorum().set(&quorum);

        let mut duplicates = false;
        self.user_mapper()
            .get_or_create_users(board.as_slice(), |user_id, new_user| {
                if !new_user {
                    duplicates = true;
                }
                self.set_user_id_to_role(user_id, UserRole::BoardMember);
            });
        require!(!duplicates, "duplicate board member");
        self.num_board_members().set(&board.len());

        require!(
            slash_amount <= required_stake,
            "slash amount must be less than or equal to required stake"
        );
        self.required_stake_amount().set(&required_stake);
        self.slash_amount().set(&slash_amount);

        Ok(())
    }

    // endpoints - owner-only

    #[endpoint]
    fn pause(&self) -> SCResult<()> {
        only_owner!(self, "only owner may pause");

        self.pause_status().set(&true);

        Ok(())
    }

    #[endpoint]
    fn unpause(&self) -> SCResult<()> {
        only_owner!(self, "only owner may unpause");

        self.pause_status().set(&false);

        Ok(())
    }

    #[endpoint(proposeSlashUser)]
    fn propose_slash_user(&self, user_address: Address) -> SCResult<usize> {
        require!(
            self.user_role(&user_address) == UserRole::BoardMember,
            "can only slash board members"
        );

        self.propose_action(Action::SlashUser(user_address))
    }

    // endpoints

    /// Allows the contract to receive funds even if it is marked as unpayable in the protocol.
    #[payable("*")]
    #[endpoint]
    fn deposit(&self) {}

    #[payable("EGLD")]
    #[endpoint]
    fn stake(&self, #[payment] payment: BigUint) -> SCResult<()> {
        let caller = self.get_caller();
        let caller_role = self.user_role(&caller);
        require!(
            caller_role == UserRole::BoardMember,
            "Only board members can stake"
        );

        let mut amount_staked = self.amount_staked(&caller).get();
        amount_staked += payment;
        self.amount_staked(&caller).set(&amount_staked);

        Ok(())
    }

    #[endpoint]
    fn unstake(&self, amount: BigUint) -> SCResult<()> {
        let caller = self.get_caller();
        let amount_staked = self.amount_staked(&caller).get();
        require!(
            amount <= amount_staked,
            "can't unstake more than amount staked"
        );

        let remaining_stake = &amount_staked - &amount;
        if self.user_role(&caller) == UserRole::BoardMember {
            let required_stake_amount = self.required_stake_amount().get();
            require!(
                remaining_stake >= required_stake_amount,
                "can't unstake, must keep minimum amount as insurance"
            );
        }

        self.amount_staked(&caller).set(&remaining_stake);
        self.send().direct_egld(&caller, &amount, &[]);

        Ok(())
    }

    /// Used by board members to sign actions.
    #[endpoint]
    fn sign(&self, action_id: usize) -> SCResult<()> {
        require!(
            !self.action_mapper().item_is_empty_unchecked(action_id),
            "action does not exist"
        );

        let caller_address = self.get_caller();
        let caller_id = self.user_mapper().get_user_id(&caller_address);
        let caller_role = self.get_user_id_to_role(caller_id);
        require!(caller_role.can_sign(), "only board members can sign");
        require!(self.has_enough_stake(&caller_address), "not enough stake");

        self.action_signer_ids(action_id).update(|signer_ids| {
            if !signer_ids.contains(&caller_id) {
                signer_ids.push(caller_id);
            }
        });

        Ok(())
    }

    /// Board members can withdraw their signatures if they no longer desire for the action to be executed.
    /// Actions that are left with no valid signatures can be then deleted to free up storage.
    #[endpoint]
    fn unsign(&self, action_id: usize) -> SCResult<()> {
        require!(
            !self.action_mapper().item_is_empty_unchecked(action_id),
            "action does not exist"
        );

        let caller_address = self.get_caller();
        let caller_id = self.user_mapper().get_user_id(&caller_address);
        let caller_role = self.get_user_id_to_role(caller_id);
        require!(caller_role.can_sign(), "only board members can un-sign");
        require!(self.has_enough_stake(&caller_address), "not enough stake");

        self.action_signer_ids(action_id).update(|signer_ids| {
            if let Some(signer_pos) = signer_ids
                .iter()
                .position(|&signer_id| signer_id == caller_id)
            {
                // since we don't care about the order,
                // it is ok to call swap_remove, which is O(1)
                signer_ids.swap_remove(signer_pos);
            }
        });

        Ok(())
    }

    /// Initiates board member addition process.
    /// Can also be used to promote a proposer to board member.
    #[endpoint(proposeAddBoardMember)]
    fn propose_add_board_member(&self, board_member_address: Address) -> SCResult<usize> {
        self.propose_action(Action::AddBoardMember(board_member_address))
    }

    /// Initiates proposer addition process..
    /// Can also be used to demote a board member to proposer.
    #[endpoint(proposeAddProposer)]
    fn propose_add_proposer(&self, proposer_address: Address) -> SCResult<usize> {
        self.propose_action(Action::AddProposer(proposer_address))
    }

    /// Removes user regardless of whether it is a board member or proposer.
    #[endpoint(proposeRemoveUser)]
    fn propose_remove_user(&self, user_address: Address) -> SCResult<usize> {
        self.propose_action(Action::RemoveUser(user_address))
    }

    #[endpoint(proposeChangeQuorum)]
    fn propose_change_quorum(&self, new_quorum: usize) -> SCResult<usize> {
        self.propose_action(Action::ChangeQuorum(new_quorum))
    }

    // EGLD-ESDT Swap SC calls

    #[endpoint(proposeEgldEsdtSwapSCDeploy)]
    fn propose_egld_esdt_swap_sc_deploy(&self, code: BoxedBytes) -> SCResult<usize> {
        require!(
            self.egld_esdt_swap_address().is_empty(),
            "EGLD-ESDT Swap SC was already deployed"
        );

        self.propose_action(Action::EgldEsdtSwapCall(EgldEsdtSwapCall::Deploy { code }))
    }

    #[endpoint(proposeEgldEsdtSwapWrappedEgldIssue)]
    fn propose_egld_esdt_swap_wrapped_egld_issue(
        &self,
        token_display_name: BoxedBytes,
        token_ticker: BoxedBytes,
        initial_supply: BigUint,
        issue_cost: BigUint,
    ) -> SCResult<usize> {
        sc_try!(self.require_egld_esdt_swap_deployed());

        self.propose_action(Action::EgldEsdtSwapCall(
            EgldEsdtSwapCall::IssueWrappedEgld {
                token_display_name,
                token_ticker,
                initial_supply,
                issue_cost,
            },
        ))
    }

    #[endpoint(proposeEgldEsdtSwapSetLocalMintRole)]
    fn propose_egld_esdt_swap_set_local_mint_role(&self) -> SCResult<usize> {
        sc_try!(self.require_egld_esdt_swap_deployed());

        self.propose_action(Action::EgldEsdtSwapCall(EgldEsdtSwapCall::SetLocalMintRole))
    }

    #[endpoint(proposeEgldEsdtSwapMintWrappedEgld)]
    fn propose_egld_esdt_swap_mint_wrapped_egld(&self, amount: BigUint) -> SCResult<usize> {
        sc_try!(self.require_egld_esdt_swap_deployed());

        self.propose_action(Action::EgldEsdtSwapCall(
            EgldEsdtSwapCall::MintWrappedEgld { amount },
        ))
    }

    // ESDT Safe SC calls

    #[endpoint(proposeEsdtSafeScDeploy)]
    fn propose_esdt_safe_sc_deploy(
        &self,
        code: BoxedBytes,
        transaction_fee: BigUint,
        #[var_args] token_whitelist: VarArgs<TokenIdentifier>,
    ) -> SCResult<usize> {
        require!(
            self.esdt_safe_address().is_empty(),
            "ESDT Safe SC was already deployed"
        );

        self.propose_action(Action::EsdtSafeCall(EsdtSafeCall::Deploy {
            code,
            transaction_fee,
            token_whitelist: token_whitelist.into_vec(),
        }))
    }

    #[endpoint(proposeEsdtSafeSetTransactionFee)]
    fn propose_esdt_safe_set_transaction_fee(&self, transaction_fee: BigUint) -> SCResult<usize> {
        sc_try!(self.require_esdt_safe_deployed());

        self.propose_action(Action::EsdtSafeCall(EsdtSafeCall::SetTransactionFee {
            transaction_fee,
        }))
    }

    #[endpoint(proposeEsdtSafeAddTokenToWhitelist)]
    fn propose_esdt_safe_add_token_to_whitelist(
        &self,
        token_id: TokenIdentifier,
    ) -> SCResult<usize> {
        sc_try!(self.require_esdt_safe_deployed());

        self.propose_action(Action::EsdtSafeCall(EsdtSafeCall::AddTokenToWhitelist {
            token_id,
        }))
    }

    #[endpoint(proposeEsdtSafeRemoveTokenFromWhitelist)]
    fn propose_esdt_safe_remove_token_from_whitelist(
        &self,
        token_id: TokenIdentifier,
    ) -> SCResult<usize> {
        sc_try!(self.require_esdt_safe_deployed());

        self.propose_action(Action::EsdtSafeCall(
            EsdtSafeCall::RemoveTokenFromWhitelist { token_id },
        ))
    }

    #[endpoint(proposeEsdtSafeGetNextPendingTransaction)]
    fn propose_esdt_safe_get_next_pending_transaction(&self) -> SCResult<usize> {
        sc_try!(self.require_esdt_safe_deployed());

        self.propose_action(Action::EsdtSafeCall(
            EsdtSafeCall::GetNextPendingTransaction,
        ))
    }

    #[endpoint(proposeEsdtSafeSetTransactionStatus)]
    fn propose_esdt_safe_set_transaction_status(
        &self,
        sender: Address,
        nonce: Nonce,
        transaction_status: TransactionStatus,
    ) -> SCResult<usize> {
        sc_try!(self.require_esdt_safe_deployed());

        self.propose_action(Action::EsdtSafeCall(EsdtSafeCall::SetTransactionStatus {
            sender,
            nonce,
            transaction_status,
        }))
    }

    #[endpoint(proposeEsdtSafeClaim)]
    fn propose_esdt_safe_claim(&self) -> SCResult<usize> {
        sc_try!(self.require_esdt_safe_deployed());

        self.propose_action(Action::EsdtSafeCall(EsdtSafeCall::Claim))
    }

    // Multi-transfer ESDT SC calls

    #[endpoint(proposeMultiTransferEsdtSCDeploy)]
    fn propose_multi_transfer_esdt_sc_deploy(&self, code: BoxedBytes) -> SCResult<usize> {
        require!(
            self.multi_transfer_esdt_address().is_empty(),
            "Multi-transfer ESDT SC was already deployed"
        );

        self.propose_action(Action::MultiTransferEsdtCall(
            MultiTransferEsdtCall::Deploy { code },
        ))
    }

    #[endpoint(proposeMultiTransferEsdtIssueEsdtToken)]
    fn propose_multi_transfer_esdt_issue_esdt_token(
        &self,
        token_display_name: BoxedBytes,
        token_ticker: BoxedBytes,
        initial_supply: BigUint,
        issue_cost: BigUint,
    ) -> SCResult<usize> {
        sc_try!(self.require_multi_transfer_esdt_deployed());

        self.propose_action(Action::MultiTransferEsdtCall(
            MultiTransferEsdtCall::IssueEsdtToken {
                token_display_name,
                token_ticker,
                initial_supply,
                issue_cost,
            },
        ))
    }

    #[endpoint(proposeMultiTransferEsdtSetLocalMintRole)]
    fn propose_multi_transfer_esdt_set_local_mint_role(
        &self,
        token_id: TokenIdentifier,
    ) -> SCResult<usize> {
        sc_try!(self.require_multi_transfer_esdt_deployed());

        self.propose_action(Action::MultiTransferEsdtCall(
            MultiTransferEsdtCall::SetLocalMintRole { token_id },
        ))
    }

    #[endpoint(proposeMultiTransferEsdtMintEsdtToken)]
    fn propose_multi_transfer_esdt_mint_esdt_token(
        &self,
        token_id: TokenIdentifier,
        amount: BigUint,
    ) -> SCResult<usize> {
        sc_try!(self.require_multi_transfer_esdt_deployed());

        self.propose_action(Action::MultiTransferEsdtCall(
            MultiTransferEsdtCall::MintEsdtToken { token_id, amount },
        ))
    }

    #[endpoint(proposeMultiTransferEsdtTransferEsdtToken)]
    fn propose_multi_transfer_esdt_transfer_esdt_token(
        &self,
        to: Address,
        token_id: TokenIdentifier,
        amount: BigUint,
    ) -> SCResult<usize> {
        sc_try!(self.require_multi_transfer_esdt_deployed());

        self.propose_action(Action::MultiTransferEsdtCall(
            MultiTransferEsdtCall::TransferEsdtToken {
                to,
                token_id,
                amount,
            },
        ))
    }

    /// Proposers and board members use this to launch signed actions.
    #[endpoint(performAction)]
    fn perform_action_endpoint(&self, action_id: usize) -> SCResult<()> {
        let caller_address = self.get_caller();
        let caller_id = self.user_mapper().get_user_id(&caller_address);
        let caller_role = self.get_user_id_to_role(caller_id);
        require!(
            caller_role.can_perform_action(),
            "only board members and proposers can perform actions"
        );
        require!(
            self.quorum_reached(action_id),
            "quorum has not been reached"
        );

        self.perform_action(action_id)
    }

    /// Clears storage pertaining to an action that is no longer supposed to be executed.
    /// Any signatures that the action received must first be removed, via `unsign`.
    /// Otherwise this endpoint would be prone to abuse.
    #[endpoint(discardAction)]
    fn discard_action(&self, action_id: usize) -> SCResult<()> {
        let caller_address = self.get_caller();
        let caller_id = self.user_mapper().get_user_id(&caller_address);
        let caller_role = self.get_user_id_to_role(caller_id);
        require!(
            caller_role.can_discard_action(),
            "only board members and proposers can discard actions"
        );
        require!(
            self.get_action_valid_signer_count(action_id) == 0,
            "cannot discard action with valid signatures"
        );

        self.clear_action(action_id);
        Ok(())
    }

    // views

    /// Iterates through all actions and retrieves those that are still pending.
    /// Serialized full action data:
    /// - the action id
    /// - the serialized action data
    /// - (number of signers followed by) list of signer addresses.
    #[view(getPendingActionFullInfo)]
    fn get_pending_action_full_info(&self) -> MultiResultVec<ActionFullInfo<BigUint>> {
        let mut result = Vec::new();
        let action_last_index = self.get_action_last_index();
        let action_mapper = self.action_mapper();
        for action_id in 1..=action_last_index {
            let action_data = action_mapper.get(action_id);
            if action_data.is_pending() {
                result.push(ActionFullInfo {
                    action_id,
                    action_data,
                    signers: self.get_action_signers(action_id),
                });
            }
        }
        result.into()
    }

    /// Returns `true` (`1`) if the user has signed the action.
    /// Does not check whether or not the user is still a board member and the signature valid.
    #[view]
    fn signed(&self, user: Address, action_id: usize) -> bool {
        let user_id = self.user_mapper().get_user_id(&user);
        if user_id == 0 {
            false
        } else {
            let signer_ids = self.action_signer_ids(action_id).get();
            signer_ids.contains(&user_id)
        }
    }

    /// Indicates user rights.
    /// `0` = no rights,
    /// `1` = can propose, but not sign,
    /// `2` = can propose and sign.
    #[view(userRole)]
    fn user_role(&self, user: &Address) -> UserRole {
        let user_id = self.user_mapper().get_user_id(&user);
        if user_id == 0 {
            UserRole::None
        } else {
            self.get_user_id_to_role(user_id)
        }
    }

    /// Lists all users that can sign actions.
    #[view(getAllBoardMembers)]
    fn get_all_board_members(&self) -> MultiResultVec<Address> {
        self.get_all_users_with_role(UserRole::BoardMember)
    }

    /// Lists all proposers that are not board members.
    #[view(getAllProposers)]
    fn get_all_proposers(&self) -> MultiResultVec<Address> {
        self.get_all_users_with_role(UserRole::Proposer)
    }

    /// Gets addresses of all users who signed an action.
    /// Does not check if those users are still board members or not,
    /// so the result may contain invalid signers.
    #[view(getActionSigners)]
    fn get_action_signers(&self, action_id: usize) -> Vec<Address> {
        self.action_signer_ids(action_id)
            .get()
            .iter()
            .map(|signer_id| self.user_mapper().get_user_address_unchecked(*signer_id))
            .collect()
    }

    /// Gets addresses of all users who signed an action and are still board members.
    /// All these signatures are currently valid.
    #[view(getActionSignerCount)]
    fn get_action_signer_count(&self, action_id: usize) -> usize {
        self.action_signer_ids(action_id).get().len()
    }

    /// It is possible for board members to lose their role.
    /// They are not automatically removed from all actions when doing so,
    /// therefore the contract needs to re-check every time when actions are performed.
    /// This function is used to validate the signers before performing an action.
    /// It also makes it easy to check before performing an action.
    #[view(getActionValidSignerCount)]
    fn get_action_valid_signer_count(&self, action_id: usize) -> usize {
        let signer_ids = self.action_signer_ids(action_id).get();
        signer_ids
            .iter()
            .filter(|signer_id| {
                let signer_role = self.get_user_id_to_role(**signer_id);
                signer_role.can_sign()
            })
            .count()
    }

    /// Returns `true` (`1`) if `getActionValidSignerCount >= getQuorum`.
    #[view(quorumReached)]
    fn quorum_reached(&self, action_id: usize) -> bool {
        let quorum = self.quorum().get();
        let valid_signers_count = self.get_action_valid_signer_count(action_id);
        valid_signers_count >= quorum
    }

    /// The index of the last proposed action.
    /// 0 means that no action was ever proposed yet.
    #[view(getActionLastIndex)]
    fn get_action_last_index(&self) -> usize {
        self.action_mapper().len()
    }

    /// Serialized action data of an action with index.
    #[view(getActionData)]
    fn get_action_data(&self, action_id: usize) -> Action<BigUint> {
        self.action_mapper().get(action_id)
    }

    // private

    fn propose_action(&self, action: Action<BigUint>) -> SCResult<usize> {
        let caller_address = self.get_caller();
        let caller_id = self.user_mapper().get_user_id(&caller_address);
        let caller_role = self.get_user_id_to_role(caller_id);
        require!(
            caller_role.can_propose(),
            "only board members and proposers can propose"
        );

        let action_id = self.action_mapper().push(&action);
        if caller_role.can_sign() {
            // also sign
            // since the action is newly created, the caller can be the only signer
            self.action_signer_ids(action_id).set(&[caller_id].to_vec());
        }

        Ok(action_id)
    }

    fn get_all_users_with_role(&self, role: UserRole) -> MultiResultVec<Address> {
        let mut result = Vec::new();
        let num_users = self.user_mapper().get_user_count();
        for user_id in 1..=num_users {
            if self.get_user_id_to_role(user_id) == role {
                if let Some(address) = self.user_mapper().get_user_address(user_id) {
                    result.push(address);
                }
            }
        }
        result.into()
    }

    /// Can be used to:
    /// - create new user (board member / proposer)
    /// - remove user (board member / proposer)
    /// - reactivate removed user
    /// - convert between board member and proposer
    /// Will keep the board size and proposer count in sync.
    fn change_user_role(&self, user_address: Address, new_role: UserRole) {
        let user_id = self.user_mapper().get_or_create_user(&user_address);
        let old_role = if user_id == 0 {
            UserRole::None
        } else {
            self.get_user_id_to_role(user_id)
        };
        self.set_user_id_to_role(user_id, new_role);

        // update board size
        #[allow(clippy::collapsible_else_if)]
        if old_role == UserRole::BoardMember {
            if new_role != UserRole::BoardMember {
                self.num_board_members().update(|value| *value -= 1);
            }
        } else {
            if new_role == UserRole::BoardMember {
                self.num_board_members().update(|value| *value += 1);
            }
        }

        // update num_proposers
        #[allow(clippy::collapsible_else_if)]
        if old_role == UserRole::Proposer {
            if new_role != UserRole::Proposer {
                self.num_proposers().update(|value| *value -= 1);
            }
        } else {
            if new_role == UserRole::Proposer {
                self.num_proposers().update(|value| *value += 1);
            }
        }
    }

    fn perform_action(&self, action_id: usize) -> SCResult<()> {
        let action = self.action_mapper().get(action_id);

        if self.pause_status().get() {
            require!(
                action.is_slash_user(),
                "Only slash user action may be performed while paused"
            );
        }

        // clean up storage
        // happens before actual execution, because the match provides the return on each branch
        // syntax aside, the async_call_raw kills contract execution so cleanup cannot happen afterwards
        self.clear_action(action_id);

        match action {
            Action::Nothing => Ok(()),
            Action::AddBoardMember(board_member_address) => {
                self.change_user_role(board_member_address, UserRole::BoardMember);
                Ok(())
            }
            Action::AddProposer(proposer_address) => {
                self.change_user_role(proposer_address, UserRole::Proposer);

                // validation required for the scenario when a board member becomes a proposer
                require!(
                    self.quorum().get() <= self.num_board_members().get(),
                    "quorum cannot exceed board size"
                );

                Ok(())
            }
            Action::RemoveUser(user_address) => {
                self.change_user_role(user_address, UserRole::None);
                let num_board_members = self.num_board_members().get();
                let num_proposers = self.num_proposers().get();
                require!(
                    num_board_members + num_proposers > 0,
                    "cannot remove all board members and proposers"
                );
                require!(
                    self.quorum().get() <= num_board_members,
                    "quorum cannot exceed board size"
                );

                Ok(())
            }
            Action::SlashUser(user_address) => {
                self.change_user_role(user_address.clone(), UserRole::None);
                let num_board_members = self.num_board_members().get();
                let num_proposers = self.num_proposers().get();

                require!(
                    num_board_members + num_proposers > 0,
                    "cannot remove all board members and proposers"
                );
                require!(
                    self.quorum().get() <= num_board_members,
                    "quorum cannot exceed board size"
                );

                let slash_amount = self.slash_amount().get();

                // remove slashed amount from user stake amount
                let mut user_stake = self.amount_staked(&user_address).get();
                user_stake -= &slash_amount;
                self.amount_staked(&user_address).set(&user_stake);

                // add it to total slashed amount pool
                let mut total_slashed_amount = self.slashed_tokens_amount().get();
                total_slashed_amount += &slash_amount;
                self.slashed_tokens_amount().set(&total_slashed_amount);

                Ok(())
            }
            Action::ChangeQuorum(new_quorum) => {
                require!(
                    new_quorum <= self.num_board_members().get(),
                    "quorum cannot exceed board size"
                );
                self.quorum().set(&new_quorum);
                
                Ok(())
            }
            Action::SCDeploy {
                amount,
                code,
                code_metadata,
                arguments,
            } => {
                let gas_left = self.get_gas_left();
                let mut arg_buffer = ArgBuffer::new();
                for arg in arguments {
                    arg_buffer.push_argument_bytes(arg.as_slice());
                }
                let new_address = self.send().deploy_contract(
                    gas_left,
                    &amount,
                    &code,
                    code_metadata,
                    &arg_buffer,
                );
                
                Ok(())
            }
        }
    }

    fn clear_action(&self, action_id: usize) {
        self.action_mapper().clear_entry_unchecked(action_id);
        self.action_signer_ids(action_id).clear();
    }

    fn has_enough_stake(&self, board_member_address: &Address) -> bool {
        let required_stake = self.required_stake_amount().get();
        let amount_staked = self.amount_staked(board_member_address).get();

        amount_staked >= required_stake
    }

    fn require_egld_esdt_swap_deployed(&self) -> SCResult<()> {
        require!(
            !self.egld_esdt_swap_address().is_empty(),
            "EGLD-ESDT Swap SC has to be deployed first"
        );
        Ok(())
    }

    fn require_esdt_safe_deployed(&self) -> SCResult<()> {
        require!(
            !self.esdt_safe_address().is_empty(),
            "ESDT Safe SC has to be deployed first"
        );
        Ok(())
    }

    fn require_multi_transfer_esdt_deployed(&self) -> SCResult<()> {
        require!(
            !self.multi_transfer_esdt_address().is_empty(),
            "Multi-transfer ESDT SC has to be deployed first"
        );
        Ok(())
    }

    // storage

    /// Minimum number of signatures needed to perform any action.
    #[view(getQuorum)]
    #[storage_mapper("quorum")]
    fn quorum(&self) -> SingleValueMapper<Self::Storage, usize>;

    #[storage_mapper("user")]
    fn user_mapper(&self) -> UserMapper<Self::Storage>;

    #[storage_get("user_role")]
    fn get_user_id_to_role(&self, user_id: usize) -> UserRole;

    #[storage_set("user_role")]
    fn set_user_id_to_role(&self, user_id: usize, user_role: UserRole);

    /// Denormalized board member count.
    /// It is kept in sync with the user list by the contract.
    #[view(getNumBoardMembers)]
    #[storage_mapper("num_board_members")]
    fn num_board_members(&self) -> SingleValueMapper<Self::Storage, usize>;

    /// Denormalized proposer count.
    /// It is kept in sync with the user list by the contract.
    #[view(getNumProposers)]
    #[storage_mapper("num_proposers")]
    fn num_proposers(&self) -> SingleValueMapper<Self::Storage, usize>;

    #[storage_mapper("action_data")]
    fn action_mapper(&self) -> VecMapper<Self::Storage, Action<BigUint>>;

    #[storage_mapper("action_signer_ids")]
    fn action_signer_ids(&self, action_id: usize) -> SingleValueMapper<Self::Storage, Vec<usize>>;

    /// The required amount to stake for accepting relayer position
    #[view(getRequiredStakeAmount)]
    #[storage_mapper("requiredStakeAmount")]
    fn required_stake_amount(&self) -> SingleValueMapper<Self::Storage, BigUint>;

    /// Staked amount by each board member.
    #[view(getAmountStaked)]
    #[storage_mapper("amountStaked")]
    fn amount_staked(
        &self,
        board_member_address: &Address,
    ) -> SingleValueMapper<Self::Storage, BigUint>;

    /// Amount of stake slashed if a relayer is misbehaving
    #[view(getSlashAmount)]
    #[storage_mapper("slashAmount")]
    fn slash_amount(&self) -> SingleValueMapper<Self::Storage, BigUint>;

    /// Total slashed tokens accumulated
    #[view(getSlashedTokensAmount)]
    #[storage_mapper("slashedTokensAmount")]
    fn slashed_tokens_amount(&self) -> SingleValueMapper<Self::Storage, BigUint>;

    #[view(isPaused)]
    #[storage_mapper("pauseStatus")]
    fn pause_status(&self) -> SingleValueMapper<Self::Storage, bool>;

    // SC addresses

    #[view(getEgldEsdtSwapAddress)]
    #[storage_mapper("egldEsdtSwapAddress")]
    fn egld_esdt_swap_address(&self) -> SingleValueMapper<Self::Storage, Address>;

    #[view(getEsdtSafeAddress)]
    #[storage_mapper("esdtSafeAddress")]
    fn esdt_safe_address(&self) -> SingleValueMapper<Self::Storage, Address>;

    #[view(getMultiTransferEsdtAddress)]
    #[storage_mapper("multiTransferEsdtAddress")]
    fn multi_transfer_esdt_address(&self) -> SingleValueMapper<Self::Storage, Address>;
}
