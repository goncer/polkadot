// Copyright 2017-2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

//! Over-bridge messaging support for Kusama <> Polkadot bridge.

use crate::{AccountId, Balance, Call, Origin, OriginCaller, Runtime};

use bp_messages::{
	source_chain::{LaneMessageVerifier, SenderOrigin, TargetHeaderChain},
	target_chain::{ProvedMessages, SourceHeaderChain},
	InboundLaneData, LaneId, Message, MessageNonce, OutboundLaneData,
	Parameter as MessagesParameter,
};
use bp_runtime::{Chain, ChainId, KUSAMA_CHAIN_ID, POLKADOT_CHAIN_ID};
use bridge_runtime_common::messages::{
	source as messages_source, target as messages_target, transaction_payment,
	BridgedChainWithMessages, ChainWithMessages, MessageBridge, MessageTransaction,
	ThisChainWithMessages,
};
use frame_support::{
	parameter_types,
	traits::{Contains, Get},
	weights::{DispatchClass, Weight, WeightToFeePolynomial},
	RuntimeDebug,
};
use parity_scale_codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_runtime::{traits::Saturating, FixedPointNumber, FixedU128};
use sp_std::{convert::TryFrom, ops::RangeInclusive};

#[cfg(feature = "runtime-benchmarks")]
use crate::{Balances, Event};
#[cfg(feature = "runtime-benchmarks")]
use bp_polkadot::{Hasher, Header};
#[cfg(feature = "runtime-benchmarks")]
use bridge_runtime_common::messages_benchmarking::{
	dispatch_account, prepare_message_delivery_proof, prepare_message_proof,
	prepare_outbound_message,
};
#[cfg(feature = "runtime-benchmarks")]
use frame_support::traits::Currency;
#[cfg(feature = "runtime-benchmarks")]
use pallet_bridge_messages::benchmarking::{
	Config as MessagesConfig, MessageDeliveryProofParams, MessageParams, MessageProofParams,
};

/// Initial value of `PolkadotToKusamaConversionRate` parameter.
pub const INITIAL_POLKADOT_TO_KUSAMA_CONVERSION_RATE: FixedU128 =
	FixedU128::from_inner(FixedU128::DIV);
/// Initial value of `PolkadotFeeMultiplier` parameter.
pub const INITIAL_POLKADOT_FEE_MULTIPLIER: FixedU128 = FixedU128::from_inner(FixedU128::DIV);

parameter_types! {
	/// Polkadot (DOT) to Kusama (KSM) conversion rate.
	pub storage PolkadotToKusamaConversionRate: FixedU128 = INITIAL_POLKADOT_TO_KUSAMA_CONVERSION_RATE;
	/// Fee multiplier at Polkadot.
	pub storage PolkadotFeeMultiplier: FixedU128 = INITIAL_POLKADOT_FEE_MULTIPLIER;
	/// The only Kusama account that is allowed to send messages to Polkadot.
	pub storage AllowedMessageSender: Option<bp_kusama::AccountId> = None;
}

/// Message payload for Kusama -> Polkadot messages.
pub type ToPolkadotMessagePayload =
	messages_source::FromThisChainMessagePayload<WithPolkadotMessageBridge>;

/// Message payload for Polkadot -> Kusama messages.
pub type FromPolkadotMessagePayload =
	messages_target::FromBridgedChainMessagePayload<WithPolkadotMessageBridge>;

/// Encoded Kusama Call as it comes from Polkadot.
pub type FromPolkadotEncodedCall = messages_target::FromBridgedChainEncodedMessageCall<crate::Call>;

/// Call-dispatch based message dispatch for Polkadot -> Kusama messages.
pub type FromPolkadotMessageDispatch = messages_target::FromBridgedChainMessageDispatch<
	WithPolkadotMessageBridge,
	crate::Runtime,
	pallet_balances::Pallet<Runtime>,
	crate::PolkadotMessagesDispatchInstance,
>;

/// Error that happens when message is sent by anyone but `AllowedMessageSender`.
#[cfg(not(feature = "runtime-benchmarks"))]
const NOT_ALLOWED_MESSAGE_SENDER: &str = "Cannot accept message from this account";
/// Error that happens when we are receiving incoming message via unexpected lane.
const INBOUND_LANE_DISABLED: &str = "The inbound message lane is disaled.";

/// Message verifier for Kusama -> Polkadot messages.
#[derive(RuntimeDebug)]
pub struct ToPolkadotMessageVerifier;

impl LaneMessageVerifier<Origin, bp_kusama::AccountId, ToPolkadotMessagePayload, bp_kusama::Balance>
	for ToPolkadotMessageVerifier
{
	type Error = &'static str;

	fn verify_message(
		submitter: &Origin,
		delivery_and_dispatch_fee: &bp_kusama::Balance,
		lane: &LaneId,
		lane_outbound_data: &OutboundLaneData,
		payload: &ToPolkadotMessagePayload,
	) -> Result<(), Self::Error> {
		let allowed_sender = AllowedMessageSender::get();
		// for benchmarks we're still interested in this additional storage, read, but we don't
		// want actual checks
		#[cfg(feature = "runtime-benchmarks")]
		drop(allowed_sender);
		// outside of benchmarks, we only allow messages to be sent by given account
		#[cfg(not(feature = "runtime-benchmarks"))]
		{
			match allowed_sender {
				Some(ref allowed_sender)
					if submitter.linked_account().as_ref() == Some(allowed_sender) =>
					(),
				_ => return Err(NOT_ALLOWED_MESSAGE_SENDER),
			}
		}

		// perform other checks
		messages_source::FromThisChainMessageVerifier::<WithPolkadotMessageBridge>::verify_message(
			submitter,
			delivery_and_dispatch_fee,
			lane,
			lane_outbound_data,
			payload,
		)
	}
}

/// Kusama <-> Polkadot message bridge.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct WithPolkadotMessageBridge;

impl MessageBridge for WithPolkadotMessageBridge {
	const RELAYER_FEE_PERCENT: u32 = 10;
	const THIS_CHAIN_ID: ChainId = KUSAMA_CHAIN_ID;
	const BRIDGED_CHAIN_ID: ChainId = POLKADOT_CHAIN_ID;
	const BRIDGED_MESSAGES_PALLET_NAME: &'static str = bp_kusama::WITH_KUSAMA_MESSAGES_PALLET_NAME;

	type ThisChain = Kusama;
	type BridgedChain = Polkadot;

	fn bridged_balance_to_this_balance(
		bridged_balance: bp_polkadot::Balance,
		polkadot_to_kusama_conversion_rate_override: Option<FixedU128>,
	) -> bp_kusama::Balance {
		let conversion_rate = polkadot_to_kusama_conversion_rate_override
			.unwrap_or_else(|| PolkadotToKusamaConversionRate::get());
		bp_kusama::Balance::try_from(conversion_rate.saturating_mul_int(bridged_balance))
			.unwrap_or(bp_kusama::Balance::MAX)
	}
}

/// Kusama from messages point of view.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct Kusama;

impl ChainWithMessages for Kusama {
	type Hash = bp_kusama::Hash;
	type AccountId = bp_kusama::AccountId;
	type Signer = bp_kusama::AccountPublic;
	type Signature = bp_kusama::Signature;
	type Weight = Weight;
	type Balance = bp_kusama::Balance;
}

impl ThisChainWithMessages for Kusama {
	type Call = crate::Call;
	type Origin = crate::Origin;

	fn is_message_accepted(submitter: &crate::Origin, lane: &LaneId) -> bool {
		*lane == [0, 0, 0, 0] && submitter.linked_account().is_some()
	}

	fn maximal_pending_messages_at_outbound_lane() -> MessageNonce {
		bp_polkadot::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX
	}

	fn estimate_delivery_confirmation_transaction() -> MessageTransaction<Weight> {
		let inbound_data_size = InboundLaneData::<bp_kusama::AccountId>::encoded_size_hint(
			bp_kusama::MAXIMAL_ENCODED_ACCOUNT_ID_SIZE,
			1,
			1,
		)
		.unwrap_or(u32::MAX);

		MessageTransaction {
			dispatch_weight: bp_kusama::MAX_SINGLE_MESSAGE_DELIVERY_CONFIRMATION_TX_WEIGHT,
			size: inbound_data_size
				.saturating_add(bp_polkadot::EXTRA_STORAGE_PROOF_SIZE)
				.saturating_add(bp_kusama::TX_EXTRA_BYTES),
		}
	}

	fn transaction_payment(transaction: MessageTransaction<Weight>) -> bp_kusama::Balance {
		// `transaction` may represent transaction from the future, when multiplier value will
		// be larger, so let's use slightly increased value
		let multiplier = FixedU128::saturating_from_rational(110, 100)
			.saturating_mul(pallet_transaction_payment::Pallet::<Runtime>::next_fee_multiplier());
		let per_byte_fee = crate::TransactionByteFee::get();
		transaction_payment(
			bp_kusama::BlockWeights::get().get(DispatchClass::Normal).base_extrinsic,
			per_byte_fee,
			multiplier,
			|weight| crate::WeightToFee::calc(&weight),
			transaction,
		)
	}
}

/// Polkadot from messages point of view.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct Polkadot;

impl ChainWithMessages for Polkadot {
	type Hash = bp_polkadot::Hash;
	type AccountId = bp_polkadot::AccountId;
	type Signer = bp_polkadot::AccountPublic;
	type Signature = bp_polkadot::Signature;
	type Weight = Weight;
	type Balance = bp_polkadot::Balance;
}

impl BridgedChainWithMessages for Polkadot {
	fn maximal_extrinsic_size() -> u32 {
		bp_polkadot::Polkadot::max_extrinsic_size()
	}

	fn message_weight_limits(_message_payload: &[u8]) -> RangeInclusive<Weight> {
		// we don't want to relay too large messages + keep reserve for future upgrades
		let upper_limit = messages_target::maximal_incoming_message_dispatch_weight(
			bp_polkadot::Polkadot::max_extrinsic_weight(),
		);

		// this bridge may be used to deliver all kind of messages, so we're not making any assumptions about
		// minimal dispatch weight here

		0..=upper_limit
	}

	fn estimate_delivery_transaction(
		message_payload: &[u8],
		include_pay_dispatch_fee_cost: bool,
		message_dispatch_weight: Weight,
	) -> MessageTransaction<Weight> {
		let message_payload_len = u32::try_from(message_payload.len()).unwrap_or(u32::MAX);
		let extra_bytes_in_payload = Weight::from(message_payload_len)
			.saturating_sub(pallet_bridge_messages::EXPECTED_DEFAULT_MESSAGE_LENGTH.into());

		MessageTransaction {
			dispatch_weight: extra_bytes_in_payload
				.saturating_mul(bp_polkadot::ADDITIONAL_MESSAGE_BYTE_DELIVERY_WEIGHT)
				.saturating_add(bp_polkadot::DEFAULT_MESSAGE_DELIVERY_TX_WEIGHT)
				.saturating_sub(if include_pay_dispatch_fee_cost {
					0
				} else {
					bp_polkadot::PAY_INBOUND_DISPATCH_FEE_WEIGHT
				})
				.saturating_add(message_dispatch_weight),
			size: message_payload_len
				.saturating_add(bp_kusama::EXTRA_STORAGE_PROOF_SIZE)
				.saturating_add(bp_polkadot::TX_EXTRA_BYTES),
		}
	}

	fn transaction_payment(transaction: MessageTransaction<Weight>) -> bp_polkadot::Balance {
		// we don't have a direct access to the value of multiplier of Polkadot chain
		// => it is a messages module parameter
		let multiplier = PolkadotFeeMultiplier::get();
		let per_byte_fee = bp_polkadot::TRANSACTION_BYTE_FEE;
		transaction_payment(
			bp_polkadot::BlockWeights::get().get(DispatchClass::Normal).base_extrinsic,
			per_byte_fee,
			multiplier,
			|weight| bp_polkadot::WeightToFee::calc(&weight),
			transaction,
		)
	}
}

impl TargetHeaderChain<ToPolkadotMessagePayload, bp_polkadot::AccountId> for Polkadot {
	type Error = &'static str;
	type MessagesDeliveryProof =
		messages_source::FromBridgedChainMessagesDeliveryProof<bp_polkadot::Hash>;

	fn verify_message(payload: &ToPolkadotMessagePayload) -> Result<(), Self::Error> {
		messages_source::verify_chain_message::<WithPolkadotMessageBridge>(payload)
	}

	fn verify_messages_delivery_proof(
		proof: Self::MessagesDeliveryProof,
	) -> Result<(LaneId, InboundLaneData<bp_kusama::AccountId>), Self::Error> {
		messages_source::verify_messages_delivery_proof::<
			WithPolkadotMessageBridge,
			Runtime,
			crate::PolkadotGrandpaInstance,
		>(proof)
	}
}

impl SourceHeaderChain<bp_polkadot::Balance> for Polkadot {
	type Error = &'static str;
	type MessagesProof = messages_target::FromBridgedChainMessagesProof<bp_polkadot::Hash>;

	fn verify_messages_proof(
		proof: Self::MessagesProof,
		messages_count: u32,
	) -> Result<ProvedMessages<Message<bp_polkadot::Balance>>, Self::Error> {
		messages_target::verify_messages_proof::<
			WithPolkadotMessageBridge,
			Runtime,
			crate::PolkadotGrandpaInstance,
		>(proof, messages_count)
		.and_then(verify_inbound_messages_lane)
	}
}

/// Verify that lanes of inbound messages are enabled.
fn verify_inbound_messages_lane(
	messages: ProvedMessages<Message<bp_polkadot::Balance>>,
) -> Result<ProvedMessages<Message<bp_polkadot::Balance>>, &'static str> {
	let allowed_incoming_lanes = [[0, 0, 0, 0]];
	if messages.keys().any(|lane_id| !allowed_incoming_lanes.contains(lane_id)) {
		return Err(INBOUND_LANE_DISABLED)
	}
	Ok(messages)
}

impl SenderOrigin<AccountId> for Origin {
	fn linked_account(&self) -> Option<AccountId> {
		match self.caller {
			// in benchmarks we accept messages from regular users
			#[cfg(feature = "runtime-benchmarks")]
			crate::OriginCaller::system(frame_system::RawOrigin::Signed(ref submitter)) =>
				Some(submitter.clone()),

			_ => map_council_origin(&self.caller),
		}
	}
}

fn map_council_origin(origin: &OriginCaller) -> Option<AccountId> {
	match *origin {
		OriginCaller::Council(_) => AllowedMessageSender::get(),
		_ => None,
	}
}

/// Kusama <> Polkadot messages pallet parameters.
#[derive(RuntimeDebug, Clone, Encode, Decode, PartialEq, Eq, TypeInfo)]
pub enum WithPolkadotMessageBridgeParameter {
	/// The conversion formula we use is: `KusamaTokens = PolkadotTokens * conversion_rate`.
	PolkadotToKusamaConversionRate(FixedU128),
	/// Fee multiplier at the Polkadot chain.
	PolkadotFeeMultiplier(FixedU128),
	/// The only Kusama account that is allowed to send messages to Polkadot.
	AllowedMessageSender(Option<bp_kusama::AccountId>),
}

impl MessagesParameter for WithPolkadotMessageBridgeParameter {
	fn save(&self) {
		match *self {
			WithPolkadotMessageBridgeParameter::PolkadotToKusamaConversionRate(
				ref conversion_rate,
			) => {
				PolkadotToKusamaConversionRate::set(conversion_rate);
			},
			WithPolkadotMessageBridgeParameter::PolkadotFeeMultiplier(ref fee_multiplier) => {
				PolkadotFeeMultiplier::set(fee_multiplier);
			},
			WithPolkadotMessageBridgeParameter::AllowedMessageSender(ref message_sender) => {
				AllowedMessageSender::set(message_sender);
			},
		}
	}
}

/// The cost of delivery confirmation transaction.
pub struct GetDeliveryConfirmationTransactionFee;

impl Get<bp_kusama::Balance> for GetDeliveryConfirmationTransactionFee {
	fn get() -> Balance {
		<Kusama as ThisChainWithMessages>::transaction_payment(
			Kusama::estimate_delivery_confirmation_transaction(),
		)
	}
}

/// Call filter for messages that are coming from Polkadot.
pub struct FromPolkadotCallFilter;

impl Contains<Call> for FromPolkadotCallFilter {
	fn contains(call: &Call) -> bool {
		#[cfg(feature = "runtime-benchmarks")]
		{
			drop(call);
			true
		}
		#[cfg(not(feature = "runtime-benchmarks"))]
		matches!(call, Call::Balances(pallet_balances::Call::transfer { .. }))
	}
}

#[cfg(feature = "runtime-benchmarks")]
impl MessagesConfig<crate::WithPolkadotMessagesInstance> for Runtime {
	fn maximal_message_size() -> u32 {
		messages_source::maximal_message_size::<WithPolkadotMessageBridge>()
	}

	fn bridged_relayer_id() -> Self::InboundRelayer {
		[0u8; 32].into()
	}

	fn account_balance(account: &Self::AccountId) -> Self::OutboundMessageFee {
		Balances::free_balance(account)
	}

	fn endow_account(account: &Self::AccountId) {
		Balances::make_free_balance_be(account, Balance::MAX / 100);
	}

	fn prepare_outbound_message(
		params: MessageParams<Self::AccountId>,
	) -> (ToPolkadotMessagePayload, bp_kusama::Balance) {
		(prepare_outbound_message::<WithPolkadotMessageBridge>(params), Self::message_fee())
	}

	fn prepare_message_proof(
		params: MessageProofParams,
	) -> (messages_target::FromBridgedChainMessagesProof<crate::Hash>, bp_messages::Weight) {
		Self::endow_account(&dispatch_account::<WithPolkadotMessageBridge>());
		prepare_message_proof::<Runtime, (), (), WithPolkadotMessageBridge, Header, Hasher>(
			params,
			&crate::VERSION,
			bp_kusama::Balance::MAX / 100,
		)
	}

	fn prepare_message_delivery_proof(
		params: MessageDeliveryProofParams<Self::AccountId>,
	) -> messages_source::FromBridgedChainMessagesDeliveryProof<crate::Hash> {
		prepare_message_delivery_proof::<Runtime, (), WithPolkadotMessageBridge, Header, Hasher>(
			params,
		)
	}

	fn is_message_dispatched(nonce: bp_messages::MessageNonce) -> bool {
		frame_system::Pallet::<Runtime>::events()
			.into_iter()
			.map(|event_record| event_record.event)
			.any(|event| matches!(
				event,
				Event::BridgePolkadotMessagesDispatch(pallet_bridge_dispatch::Event::<Runtime, _>::MessageDispatched(
					_, ([0, 0, 0, 0], nonce_from_event), _,
				)) if nonce_from_event == nonce
			))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::*;
	use bp_messages::{target_chain::ProvedLaneMessages, MessageData, MessageKey};
	use frame_support::weights::GetDispatchInfo;

	fn message_payload(sender: bp_kusama::AccountId) -> ToPolkadotMessagePayload {
		let call = Call::Balances(pallet_balances::Call::<Runtime>::transfer {
			dest: bp_polkadot::AccountId::from([0u8; 32]).into(),
			value: 10_000_000_000,
		});
		let weight = call.get_dispatch_info().weight;
		bp_message_dispatch::MessagePayload {
			spec_version: 4242,
			weight,
			origin: bp_message_dispatch::CallOrigin::SourceAccount(sender),
			dispatch_fee_payment: bp_runtime::messages::DispatchFeePayment::AtSourceChain,
			call: call.encode(),
		}
	}

	#[test]
	fn ensure_kusama_message_lane_weights_are_correct() {
		type Weights = crate::weights::pallet_bridge_messages::WeightInfo<Runtime>;

		pallet_bridge_messages::ensure_weights_are_correct::<Weights>(
			bp_kusama::DEFAULT_MESSAGE_DELIVERY_TX_WEIGHT,
			bp_kusama::ADDITIONAL_MESSAGE_BYTE_DELIVERY_WEIGHT,
			bp_kusama::MAX_SINGLE_MESSAGE_DELIVERY_CONFIRMATION_TX_WEIGHT,
			bp_kusama::PAY_INBOUND_DISPATCH_FEE_WEIGHT,
			<Runtime as frame_system::Config>::DbWeight::get(),
		);

		let max_incoming_message_proof_size = bp_polkadot::EXTRA_STORAGE_PROOF_SIZE.saturating_add(
			messages_target::maximal_incoming_message_size(bp_kusama::Kusama::max_extrinsic_size()),
		);
		pallet_bridge_messages::ensure_able_to_receive_message::<Weights>(
			bp_kusama::Kusama::max_extrinsic_size(),
			bp_kusama::Kusama::max_extrinsic_weight(),
			max_incoming_message_proof_size,
			messages_target::maximal_incoming_message_dispatch_weight(
				bp_kusama::Kusama::max_extrinsic_weight(),
			),
		);

		let max_incoming_inbound_lane_data_proof_size =
			bp_messages::InboundLaneData::<()>::encoded_size_hint(
				bp_kusama::MAXIMAL_ENCODED_ACCOUNT_ID_SIZE,
				bp_polkadot::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX as _,
				bp_polkadot::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX as _,
			)
			.unwrap_or(u32::MAX);
		pallet_bridge_messages::ensure_able_to_receive_confirmation::<Weights>(
			bp_kusama::Kusama::max_extrinsic_size(),
			bp_kusama::Kusama::max_extrinsic_weight(),
			max_incoming_inbound_lane_data_proof_size,
			bp_polkadot::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX,
			bp_polkadot::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX,
			<Runtime as frame_system::Config>::DbWeight::get(),
		);
	}

	#[test]
	fn message_by_invalid_submitter_are_rejected() {
		sp_io::TestExternalities::new(Default::default()).execute_with(|| {
			let invalid_sender = bp_kusama::AccountId::from([1u8; 32]);
			let allowed_sender = bp_kusama::AccountId::from([2u8; 32]);
			let council_member = bp_kusama::AccountId::from([3u8; 32]);
			AllowedMessageSender::set(&Some(allowed_sender.clone()));

			assert_eq!(
				map_council_origin(&frame_system::RawOrigin::Signed(invalid_sender.clone()).into()),
				None,
			);
			assert_eq!(
				map_council_origin(&frame_system::RawOrigin::Signed(allowed_sender.clone()).into()),
				None,
			);
			assert_eq!(
				map_council_origin(
					&OriginCaller::Council(pallet_collective::RawOrigin::Members(1, 1)).into()
				),
				Some(allowed_sender.clone()),
			);

			assert_eq!(
				ToPolkadotMessageVerifier::verify_message(
					&OriginCaller::Council(pallet_collective::RawOrigin::Members(1, 1)).into(),
					&bp_kusama::Balance::MAX,
					&Default::default(),
					&Default::default(),
					&message_payload(council_member.clone()),
				),
				Ok(()),
			);
			assert_eq!(
				ToPolkadotMessageVerifier::verify_message(
					&OriginCaller::Council(pallet_collective::RawOrigin::Member(
						council_member.clone()
					))
					.into(),
					&bp_kusama::Balance::MAX,
					&Default::default(),
					&Default::default(),
					&message_payload(council_member),
				),
				Ok(()),
			);
		});
	}

	fn proved_messages(lane_id: LaneId) -> ProvedMessages<Message<bp_polkadot::Balance>> {
		vec![(
			lane_id,
			ProvedLaneMessages {
				lane_state: None,
				messages: vec![Message {
					key: MessageKey { lane_id, nonce: 0 },
					data: MessageData { payload: vec![], fee: 0 },
				}],
			},
		)]
		.into_iter()
		.collect()
	}

	#[test]
	fn verify_inbound_messages_lane_succeeds() {
		assert_eq!(
			verify_inbound_messages_lane(proved_messages([0, 0, 0, 0])),
			Ok(proved_messages([0, 0, 0, 0])),
		);
	}

	#[test]
	fn verify_inbound_messages_lane_fails() {
		assert_eq!(
			verify_inbound_messages_lane(proved_messages([0, 0, 0, 1])),
			Err(INBOUND_LANE_DISABLED),
		);

		let proved_messages = proved_messages([0, 0, 0, 0])
			.into_iter()
			.chain(proved_messages([0, 0, 0, 1]))
			.collect();
		assert_eq!(verify_inbound_messages_lane(proved_messages), Err(INBOUND_LANE_DISABLED),);
	}
}
