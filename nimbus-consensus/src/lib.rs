// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! The nimbus consensus client-side worker
//!
//! It queries the in-runtime filter to determine whether any keys
//! stored in its keystore are eligible to author at this slot. If it has an eligible
//! key it authors.

use cumulus_client_consensus_common::{
	ParachainBlockImport, ParachainCandidate, ParachainConsensus,
};
use cumulus_primitives_core::{
	relay_chain::v1::{Block as PBlock, Hash as PHash, ParachainHost},
	ParaId, PersistedValidationData,
};
pub use import_queue::import_queue;
use log::{info, warn, debug};
use parking_lot::Mutex;
use polkadot_client::ClientHandle;
use sc_client_api::Backend;
use sp_api::{ProvideRuntimeApi, BlockId, ApiExt};
use sp_application_crypto::CryptoTypePublicPair;
use sp_consensus::{
	BlockOrigin, EnableProofRecording, Environment,
	ProofRecording, Proposal, Proposer,
};
use sc_consensus::{BlockImport, BlockImportParams};
use sp_inherents::{CreateInherentDataProviders, InherentData, InherentDataProvider};
use sp_runtime::traits::{Block as BlockT, HashFor, Header as HeaderT};
use std::{marker::PhantomData, sync::Arc, time::Duration};
use tracing::error;
use sp_keystore::{SyncCryptoStorePtr, SyncCryptoStore};
use sp_core::crypto::Public;
//TODO Shouldn't this come from `std` and not need `sp-std` at all?
use sp_std::convert::TryInto;
use nimbus_primitives::{NimbusApi, NIMBUS_KEY_ID, NimbusId};
mod import_queue;
mod manual_seal;

const LOG_TARGET: &str = "filtering-consensus";

/// The implementation of the relay-chain provided consensus for parachains.
pub struct NimbusConsensus<B, PF, BI, RClient, RBackend, ParaClient, CIDP> {
	para_id: ParaId,
	_phantom: PhantomData<B>,
	proposer_factory: Arc<Mutex<PF>>,
	create_inherent_data_providers: Arc<CIDP>,
	block_import: Arc<futures::lock::Mutex<ParachainBlockImport<BI>>>,
	relay_chain_client: Arc<RClient>,
	relay_chain_backend: Arc<RBackend>,
	parachain_client: Arc<ParaClient>,
	keystore: SyncCryptoStorePtr,
	skip_prediction: bool,
}

impl<B, PF, BI, RClient, RBackend, ParaClient, CIDP> Clone for NimbusConsensus<B, PF, BI, RClient, RBackend, ParaClient, CIDP> {
	fn clone(&self) -> Self {
		Self {
			para_id: self.para_id,
			_phantom: PhantomData,
			proposer_factory: self.proposer_factory.clone(),
			create_inherent_data_providers: self.create_inherent_data_providers.clone(),
			block_import: self.block_import.clone(),
			relay_chain_backend: self.relay_chain_backend.clone(),
			relay_chain_client: self.relay_chain_client.clone(),
			parachain_client: self.parachain_client.clone(),
			keystore: self.keystore.clone(),
			skip_prediction: self.skip_prediction,
		}
	}
}

impl<B, PF, BI, RClient, RBackend, ParaClient, CIDP> NimbusConsensus<B, PF, BI, RClient, RBackend, ParaClient, CIDP>
where
	B: BlockT,
	RClient: ProvideRuntimeApi<PBlock>,
	RClient::Api: ParachainHost<PBlock>,
	RBackend: Backend<PBlock>,
	ParaClient: ProvideRuntimeApi<B>,
	CIDP: CreateInherentDataProviders<B, (PHash, PersistedValidationData, NimbusId)>,
{
	/// Create a new instance of nimbus consensus.
	pub fn new(
		para_id: ParaId,
		proposer_factory: PF,
		create_inherent_data_providers: CIDP,
		block_import: BI,
		polkadot_client: Arc<RClient>,
		polkadot_backend: Arc<RBackend>,
		parachain_client: Arc<ParaClient>,
		keystore: SyncCryptoStorePtr,
		skip_prediction: bool,
	) -> Self {
		Self {
			para_id,
			proposer_factory: Arc::new(Mutex::new(proposer_factory)),
			create_inherent_data_providers: Arc::new(create_inherent_data_providers),
			block_import: Arc::new(futures::lock::Mutex::new(ParachainBlockImport::new(
				block_import,
			))),
			relay_chain_backend: polkadot_backend,
			relay_chain_client: polkadot_client,
			parachain_client,
			keystore,
			skip_prediction,
			_phantom: PhantomData,
		}
	}

	//TODO Could this be a provided implementation now that we have this async inherent stuff?
	/// Create the data.
	async fn inherent_data(
		&self,
		parent: B::Hash,
		validation_data: &PersistedValidationData,
		relay_parent: PHash,
		author_id: NimbusId,
	) -> Option<InherentData> {
		let inherent_data_providers = self
			.create_inherent_data_providers
			.create_inherent_data_providers(parent, (relay_parent, validation_data.clone(), author_id))
			.await
			.map_err(|e| {
				tracing::error!(
					target: LOG_TARGET,
					error = ?e,
					"Failed to create inherent data providers.",
				)
			})
			.ok()?;
		
		inherent_data_providers
			.create_inherent_data()
			.map_err(|e| {
				tracing::error!(
					target: LOG_TARGET,
					error = ?e,
					"Failed to create inherent data.",
				)
			})
			.ok()
	}
}

/// Grabs any available nimbus key fro mthe keystore.
/// This may be useful in situations where you expect exactly one key
/// and intend to perform an operation with it regardless of whether it is
/// expected to be eligible. Concretely, this is used in the consensus worker
/// to implement the `skip_prediction` feature.
pub(crate) fn first_available_key(keystore: &dyn SyncCryptoStore) -> Option<CryptoTypePublicPair> {
	// Get allthe available keys
	let available_keys =
		SyncCryptoStore::keys(keystore, NIMBUS_KEY_ID)
		.expect("keystore should return the keys it has");

	// Print a more helpful message than "not eligible" when there are no keys at all.
	if available_keys.is_empty() {
		warn!(target: LOG_TARGET, "🔏 No Nimbus keys available. We will not be able to author.");
		return None;
	}

	Some(available_keys[1].clone())
}

/// Grab the first eligible nimbus key from the keystore
/// If multiple keys are eligible this function still only returns one
/// and makes no guarantees which one as that depends on the keystore's iterator behavior.
/// This is the standard way of determining which key to author with.
pub(crate) fn first_eligible_key<B: BlockT, C>(client: Arc<C>, keystore: &dyn SyncCryptoStore, parent: &B::Header, slot_number: u32) -> Option<CryptoTypePublicPair>
where
	C: ProvideRuntimeApi<B>,
	C::Api: NimbusApi<B, NimbusId>,
{
	// Get allthe available keys
	let available_keys =
		SyncCryptoStore::keys(keystore, NIMBUS_KEY_ID)
		.expect("keystore should return the keys it has");

	// Print a more helpful message than "not eligible" when there are no keys at all.
	if available_keys.is_empty() {
		warn!(target: LOG_TARGET, "🔏 No Nimbus keys available. We will not be able to author.");
		return None;
	}

	// Get `NimbusApi` version.
	let api_version = client.runtime_api()
		.api_version::<dyn NimbusApi<B, NimbusId>>(&BlockId::Hash(parent.hash()))
		.expect("Runtime api access to not error.");

	if api_version.is_none() {
		tracing::error!(
			target: LOG_TARGET, "Could not find `NimbusApi` version.",
		);
		return None;
	}
	let api_version = api_version.unwrap();

	// Iterate keys until we find an eligible one, or run out of candidates.
	// If we are skipping prediction, then we author withthe first key we find.
	// prediction skipping only really amkes sense when there is a single key in the keystore.
	//TODO I think there is a nicer way to do this than handle the type_public_pair. We only use the .1 field
	let maybe_key = available_keys.into_iter().find(|type_public_pair| {

		// Have to convert to a typed NimbusId to pass to the runtime API. Maybe this is a clue
		// That I should be passing Vec<u8> across the wasm boundary?
		if api_version >= 2 {
			client.runtime_api().can_author(
				&BlockId::Hash(parent.hash()),
				NimbusId::from_slice(&type_public_pair.1),
				slot_number,
				parent,
			)
			.expect("Author API should not return error")
		} else {
			#[allow(deprecated)]
			client.runtime_api().can_author_before_version_2(
				&BlockId::Hash(parent.hash()),
				NimbusId::from_slice(&type_public_pair.1),
				slot_number,
			)
			.expect("Author API version 2 should not return error")
		}
	});

	// If there are no eligible keys, print the log, and exit early.
	if maybe_key.is_none() {
		info!(
			target: LOG_TARGET,
			"🔮 Skipping candidate production because we are not eligible"
		);
	}

	maybe_key
}

#[async_trait::async_trait]
impl<B, PF, BI, RClient, RBackend, ParaClient, CIDP> ParachainConsensus<B>
	for NimbusConsensus<B, PF, BI, RClient, RBackend, ParaClient, CIDP>
where
	B: BlockT,
	RClient: ProvideRuntimeApi<PBlock> + Send + Sync,
	RClient::Api: ParachainHost<PBlock>,
	RBackend: Backend<PBlock>,
	BI: BlockImport<B> + Send + Sync,
	PF: Environment<B> + Send + Sync,
	PF::Proposer: Proposer<
		B,
		Transaction = BI::Transaction,
		ProofRecording = EnableProofRecording,
		Proof = <EnableProofRecording as ProofRecording>::Proof,
	>,
	ParaClient: ProvideRuntimeApi<B> + Send + Sync,
	ParaClient::Api: NimbusApi<B, NimbusId>,
	CIDP: CreateInherentDataProviders<B, (PHash, PersistedValidationData, NimbusId)>,
{
	async fn produce_candidate(
		&mut self,
		parent: &B::Header,
		relay_parent: PHash,
		validation_data: &PersistedValidationData,
	) -> Option<ParachainCandidate<B>> {
		let maybe_key = if self.skip_prediction {
			first_available_key(&*self.keystore)
		}
		else {
			first_eligible_key::<B, ParaClient>(self.parachain_client.clone(), &*self.keystore, parent, validation_data.relay_parent_number)
		};

		let type_public_pair = match maybe_key {
			Some(p) => p,
			None => { return None; }
		};

		let proposer_future = self.proposer_factory.lock().init(&parent);

		let proposer = proposer_future
			.await
			.map_err(
				|e| error!(target: LOG_TARGET, error = ?e, "Could not create proposer."),
			)
			.ok()?;

		let inherent_data = self.inherent_data(parent.hash(),&validation_data, relay_parent, NimbusId::from_slice(&type_public_pair.1)).await?;

		let Proposal {
			block,
			storage_changes,
			proof,
		} = proposer
			.propose(
				inherent_data,
				Default::default(),
				//TODO: Fix this.
				Duration::from_millis(500),
				// Set the block limit to 50% of the maximum PoV size.
				//
				// TODO: If we got benchmarking that includes that encapsulates the proof size,
				// we should be able to use the maximum pov size.
				Some((validation_data.max_pov_size / 2) as usize),
			)
			.await
			.map_err(|e| error!(target: LOG_TARGET, error = ?e, "Proposing failed."))
			.ok()?;

		let (header, extrinsics) = block.clone().deconstruct();

		let pre_hash = header.hash();

		let raw_sig = SyncCryptoStore::sign_with(
			&*self.keystore,
			NIMBUS_KEY_ID,
			&type_public_pair,
			pre_hash.as_ref(),
		)
		.expect("Keystore should be able to sign")
		.expect("We already checked that the key was present");
		
		debug!(
			target: LOG_TARGET,
			"The signature is \n{:?}", raw_sig
		);

		let signature = raw_sig
				.clone()
				.try_into().ok()?;
		
		let sig_digest = <sp_runtime::traits::DigestItemFor<B> as nimbus_primitives::digests::CompatibleDigestItem>::nimbus_seal(signature);

		let mut block_import_params = BlockImportParams::new(BlockOrigin::Own, header.clone());
		block_import_params.post_digests.push(sig_digest.clone());
		block_import_params.body = Some(extrinsics.clone());
		block_import_params.state_action = sc_consensus::StateAction::ApplyChanges(
			sc_consensus::StorageChanges::Changes(storage_changes)
		);

		// Print the same log line as slots (aura and babe)
		info!(
			"🔖 Sealed block for proposal at {}. Hash now {:?}, previously {:?}.",
			*header.number(),
			block_import_params.post_hash(),
			pre_hash,
		);

		if let Err(err) = self
			.block_import
			.lock()
			.await
			.import_block(block_import_params, Default::default())
			.await
		{
			error!(
				target: LOG_TARGET,
				at = ?parent.hash(),
				error = ?err,
				"Error importing built block.",
			);

			return None;
		}

		// Compute info about the block after the digest is added
		let mut post_header = header.clone();
		post_header.digest_mut().logs.push(sig_digest.clone());
		let post_block = B::new(post_header, extrinsics);

		// Returning the block WITH the seal for distribution around the network.
		Some(ParachainCandidate { block: post_block, proof })
	}
}

/// Paramaters of [`build_relay_chain_consensus`].
///
/// I briefly tried the async keystore approach, but decided to go sync so I can copy
/// code from Aura. Maybe after it is working, Jeremy can help me go async.
pub struct BuildNimbusConsensusParams<PF, BI, RBackend, ParaClient, CIDP> {
	pub para_id: ParaId,
	pub proposer_factory: PF,
	pub create_inherent_data_providers: CIDP,
	pub block_import: BI,
	pub relay_chain_client: polkadot_client::Client,
	pub relay_chain_backend: Arc<RBackend>,
	pub parachain_client: Arc<ParaClient>,
	pub keystore: SyncCryptoStorePtr,
	pub skip_prediction: bool,

}

/// Build the [`NimbusConsensus`].
///
/// Returns a boxed [`ParachainConsensus`].
pub fn build_nimbus_consensus<Block, PF, BI, RBackend, ParaClient, CIDP>(
	BuildNimbusConsensusParams {
		para_id,
		proposer_factory,
		create_inherent_data_providers,
		block_import,
		relay_chain_client,
		relay_chain_backend,
		parachain_client,
		keystore,
		skip_prediction,
	}: BuildNimbusConsensusParams<PF, BI, RBackend, ParaClient, CIDP>,
) -> Box<dyn ParachainConsensus<Block>>
where
	Block: BlockT,
	PF: Environment<Block> + Send + Sync + 'static,
	PF::Proposer: Proposer<
		Block,
		Transaction = BI::Transaction,
		ProofRecording = EnableProofRecording,
		Proof = <EnableProofRecording as ProofRecording>::Proof,
	>,
	BI: BlockImport<Block> + Send + Sync + 'static,
	RBackend: Backend<PBlock> + 'static,
	// Rust bug: https://github.com/rust-lang/rust/issues/24159
	sc_client_api::StateBackendFor<RBackend, PBlock>: sc_client_api::StateBackend<HashFor<PBlock>>,
	ParaClient: ProvideRuntimeApi<Block> + Send + Sync + 'static,
	ParaClient::Api: NimbusApi<Block, NimbusId>,
	CIDP: CreateInherentDataProviders<Block, (PHash, PersistedValidationData, NimbusId)> + 'static,
{
	NimbusConsensusBuilder::new(
		para_id,
		proposer_factory,
		block_import,
		create_inherent_data_providers,
		relay_chain_client,
		relay_chain_backend,
		parachain_client,
		keystore,
		skip_prediction,
	)
	.build()
}

/// Nimbus consensus builder.
///
/// Builds a [`NimbusConsensus`] for a parachain. As this requires
/// a concrete relay chain client instance, the builder takes a [`polkadot_client::Client`]
/// that wraps this concrete instanace. By using [`polkadot_client::ExecuteWithClient`]
/// the builder gets access to this concrete instance.
struct NimbusConsensusBuilder<Block, PF, BI, RBackend, ParaClient,CIDP> {
	para_id: ParaId,
	_phantom: PhantomData<Block>,
	proposer_factory: PF,
	create_inherent_data_providers: CIDP,
	block_import: BI,
	relay_chain_backend: Arc<RBackend>,
	relay_chain_client: polkadot_client::Client,
	parachain_client: Arc<ParaClient>,
	keystore: SyncCryptoStorePtr,
	skip_prediction: bool,
}

impl<Block, PF, BI, RBackend, ParaClient, CIDP> NimbusConsensusBuilder<Block, PF, BI, RBackend, ParaClient, CIDP>
where
	Block: BlockT,
	// Rust bug: https://github.com/rust-lang/rust/issues/24159
	sc_client_api::StateBackendFor<RBackend, PBlock>: sc_client_api::StateBackend<HashFor<PBlock>>,
	PF: Environment<Block> + Send + Sync + 'static,
	PF::Proposer: Proposer<
		Block,
		Transaction = BI::Transaction,
		ProofRecording = EnableProofRecording,
		Proof = <EnableProofRecording as ProofRecording>::Proof,
	>,
	BI: BlockImport<Block> + Send + Sync + 'static,
	RBackend: Backend<PBlock> + 'static,
	ParaClient: ProvideRuntimeApi<Block> + Send + Sync + 'static,
	CIDP: CreateInherentDataProviders<Block, (PHash, PersistedValidationData, NimbusId)> + 'static,
{
	/// Create a new instance of the builder.
	fn new(
		para_id: ParaId,
		proposer_factory: PF,
		block_import: BI,
		create_inherent_data_providers: CIDP,
		relay_chain_client: polkadot_client::Client,
		relay_chain_backend: Arc<RBackend>,
		parachain_client: Arc<ParaClient>,
		keystore: SyncCryptoStorePtr,
		skip_prediction: bool,
	) -> Self {
		Self {
			para_id,
			_phantom: PhantomData,
			proposer_factory,
			block_import,
			create_inherent_data_providers,
			relay_chain_backend,
			relay_chain_client,
			parachain_client,
			keystore,
			skip_prediction,
		}
	}

	/// Build the nimbus consensus.
	fn build(self) -> Box<dyn ParachainConsensus<Block>>
	where
		ParaClient::Api: NimbusApi<Block, NimbusId>,
	{
		self.relay_chain_client.clone().execute_with(self)
	}
}

impl<Block, PF, BI, RBackend, ParaClient, CIDP> polkadot_client::ExecuteWithClient
	for NimbusConsensusBuilder<Block, PF, BI, RBackend, ParaClient, CIDP>
where
	Block: BlockT,
	// Rust bug: https://github.com/rust-lang/rust/issues/24159
	sc_client_api::StateBackendFor<RBackend, PBlock>: sc_client_api::StateBackend<HashFor<PBlock>>,
	PF: Environment<Block> + Send + Sync + 'static,
	PF::Proposer: Proposer<
		Block,
		Transaction = BI::Transaction,
		ProofRecording = EnableProofRecording,
		Proof = <EnableProofRecording as ProofRecording>::Proof,
	>,
	BI: BlockImport<Block> + Send + Sync + 'static,
	RBackend: Backend<PBlock> + 'static,
	ParaClient: ProvideRuntimeApi<Block> + Send + Sync + 'static,
	ParaClient::Api: NimbusApi<Block, NimbusId>,
	CIDP: CreateInherentDataProviders<Block, (PHash, PersistedValidationData, NimbusId)> + 'static,
{
	type Output = Box<dyn ParachainConsensus<Block>>;

	fn execute_with_client<PClient, Api, PBackend>(self, client: Arc<PClient>) -> Self::Output
	where
		<Api as sp_api::ApiExt<PBlock>>::StateBackend: sp_api::StateBackend<HashFor<PBlock>>,
		PBackend: Backend<PBlock>,
		PBackend::State: sp_api::StateBackend<sp_runtime::traits::BlakeTwo256>,
		Api: polkadot_client::RuntimeApiCollection<StateBackend = PBackend::State>,
		PClient: polkadot_client::AbstractClient<PBlock, PBackend, Api = Api> + 'static,
		ParaClient::Api: NimbusApi<Block, NimbusId>,
	{
		Box::new(NimbusConsensus::new(
			self.para_id,
			self.proposer_factory,
			self.create_inherent_data_providers,
			self.block_import,
			client.clone(),
			self.relay_chain_backend,
			self.parachain_client,
			self.keystore,
			self.skip_prediction,
		))
	}
}