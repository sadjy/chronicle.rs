// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use bee_message::input::Input;

use super::*;
#[async_trait::async_trait]
impl<H: PermanodeBrokerScope> EventLoop<BrokerHandle<H>> for Collector {
    async fn event_loop(
        &mut self,
        _status: Result<(), Need>,
        _supervisor: &mut Option<BrokerHandle<H>>,
    ) -> Result<(), Need> {
        while let Some(event) = self.inbox.recv().await {
            match event {
                CollectorEvent::Message(message_id, mut message) => {
                    // info!("Inserting: {}", message_id.to_string());
                    // check if msg already in lru cache(if so then it's already presisted)
                    if let None = self.lru_msg.get(&message_id) {
                        // store message
                        self.insert_message(&message_id, &mut message);
                        // add it to the cache in order to not presist it again.
                        self.lru_msg.put(message_id, (self.est_ms, message));
                    }
                }
                CollectorEvent::MessageReferenced(metadata) => {
                    let ref_ms = metadata.referenced_by_milestone_index.as_ref().unwrap();
                    let _partition_id = (ref_ms % (self.collectors_count as u32)) as u8;
                    let message_id = metadata.message_id;
                    // update the est_ms to be the most recent ref_ms
                    self.est_ms.0 = *ref_ms;
                    // check if msg already in lru cache(if so then it's already presisted)
                    if let None = self.lru_msg_ref.get(&message_id) {
                        // check if msg already exist in the cache, if so we push it to solidifier
                        let cached_msg;
                        if let Some((est_ms, message)) = self.lru_msg.get_mut(&message_id) {
                            // check if est_ms is not identical to ref_ms
                            if &est_ms.0 != ref_ms {
                                todo!("delete duplicated rows");
                                // adjust est_ms to match the actual ref_ms
                                est_ms.0 = *ref_ms;
                            }
                            cached_msg = Some(message.clone());
                            // TODO push to solidifer
                        } else {
                            cached_msg = None;
                        }
                        if let Some(message) = cached_msg {
                            self.insert_message_with_metadata(&message_id.clone(), &mut message.clone(), &metadata);
                        } else {
                            // store it as metadata
                            self.insert_message_metadata(metadata.clone());
                        }
                        // add it to the cache in order to not presist it again.
                        self.lru_msg_ref.put(message_id, metadata);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Collector {
    #[cfg(feature = "filter")]
    fn get_keyspace_for_message(&self, message: &mut Message) -> PermanodeKeyspace {
        let res = futures::executor::block_on(permanode_filter::filter_messages(message));
        PermanodeKeyspace::new(res.keyspace.into_owned())
    }
    fn get_keyspace(&self) -> PermanodeKeyspace {
        // Get the first keyspace or default to "permanode"
        // In order to use multiple keyspaces, the user must
        // use filters to determine where records go
        PermanodeKeyspace::new(
            self.storage_config
                .as_ref()
                .and_then(|config| {
                    config
                        .keyspaces
                        .first()
                        .and_then(|keyspace| Some(keyspace.name.clone()))
                })
                .unwrap_or("permanode".to_owned()),
        )
    }

    fn insert_message(&mut self, message_id: &MessageId, message: &mut Message) {
        // Check if metadata already exist in the cache
        let ledger_inclusion_state;

        #[cfg(feature = "filter")]
        let keyspace = self.get_keyspace_for_message(message);
        #[cfg(not(feature = "filter"))]
        let keyspace = self.get_keyspace();

        if let Some(meta) = self.lru_msg_ref.get(message_id) {
            ledger_inclusion_state = meta.ledger_inclusion_state.clone();
            self.est_ms = MilestoneIndex(*meta.referenced_by_milestone_index.as_ref().unwrap());
            let message_tuple = (message.clone(), meta.clone());
            // store message and metadata
            self.insert(&keyspace, *message_id, message_tuple);
        } else {
            ledger_inclusion_state = None;
            self.est_ms.0 += 1;
            // store message only
            self.insert(&keyspace, *message_id, message.clone());
        };
        // Insert parents/children
        self.insert_parents(
            &message_id,
            &message.parents(),
            self.est_ms,
            ledger_inclusion_state.clone(),
        );
        // insert payload (if any)
        if let Some(payload) = message.payload() {
            self.insert_payload(&message_id, &payload, self.est_ms, ledger_inclusion_state);
        }
    }
    fn insert_parents(
        &self,
        message_id: &MessageId,
        parents: &[MessageId],
        milestone_index: MilestoneIndex,
        inclusion_state: Option<LedgerInclusionState>,
    ) {
        let partition_id = self.partitioner.partition_id(milestone_index.0);
        for parent_id in parents {
            let partitioned = Partitioned::new(*parent_id, partition_id);
            let parent_record = ParentRecord::new(milestone_index, *message_id, inclusion_state);
            self.insert(&self.get_keyspace(), partitioned, parent_record);
            // insert hint record
            let hint = Hint::<MessageId>::new(*parent_id);
            let partition = Partition::new(partition_id, *milestone_index);
            self.insert(&self.get_keyspace(), hint, partition)
        }
    }
    fn insert_payload(
        &self,
        message_id: &MessageId,
        payload: &Payload,
        milestone_index: MilestoneIndex,
        inclusion_state: Option<LedgerInclusionState>,
    ) {
        match payload {
            Payload::Indexation(indexation) => {
                info!(
                    "Inserting Hashed index: {}",
                    String::from_utf8_lossy(indexation.index())
                );
                self.insert_hashed_index(message_id, indexation.hash(), milestone_index, inclusion_state);
            }
            Payload::Transaction(transaction) => {
                self.insert_transaction(message_id, transaction, inclusion_state, milestone_index)
            }
            // remaining payload types
            _ => {
                todo!("impl insert for remaining payloads")
            }
        }
    }
    fn insert_hashed_index(
        &self,
        message_id: &MessageId,
        hashed_index: HashedIndex,
        milestone_index: MilestoneIndex,
        inclusion_state: Option<LedgerInclusionState>,
    ) {
        let partition_id = self.partitioner.partition_id(milestone_index.0);
        let partitioned = Partitioned::new(hashed_index, partition_id);
        let hashed_index_record = HashedIndexRecord::new(milestone_index, *message_id, inclusion_state);
        self.insert(&self.get_keyspace(), partitioned, hashed_index_record);
        // insert hint record
        let hint = Hint::<HashedIndex>::new(hashed_index);
        let partition = Partition::new(partition_id, *milestone_index);
        self.insert(&self.get_keyspace(), hint, partition)
    }
    fn insert_message_metadata(&mut self, metadata: MessageMetadataObj) {
        let message_id = metadata.message_id;
        // store message and metadata
        self.insert(&self.get_keyspace(), message_id, metadata.clone());
        // Insert parents/children
        let parents = metadata.parent_message_ids;
        self.insert_parents(
            &message_id,
            &parents.as_slice(),
            self.est_ms,
            metadata.ledger_inclusion_state.clone(),
        );
    }
    fn insert_message_with_metadata(
        &mut self,
        message_id: &MessageId,
        message: &mut Message,
        metadata: &MessageMetadataObj,
    ) {
        #[cfg(feature = "filter")]
        let keyspace = self.get_keyspace_for_message(message);
        #[cfg(not(feature = "filter"))]
        let keyspace = self.get_keyspace();

        let message_tuple = (message.clone(), metadata.clone());
        // store message and metadata
        self.insert(&keyspace, *message_id, message_tuple);
        // Insert parents/children
        self.insert_parents(
            &message_id,
            &message.parents(),
            self.est_ms,
            metadata.ledger_inclusion_state.clone(),
        );
        // insert payload (if any)
        if let Some(payload) = message.payload() {
            self.insert_payload(
                &message_id,
                &payload,
                self.est_ms,
                metadata.ledger_inclusion_state.clone(),
            );
        }
    }
    fn insert_transaction(
        &self,
        message_id: &MessageId,
        transaction: &Box<TransactionPayload>,
        ledger_inclusion_state: Option<LedgerInclusionState>,
        milestone_index: MilestoneIndex,
    ) {
        let transaction_id = transaction.id();
        let unlock_blocks = transaction.unlock_blocks();
        let confirmed_milestone_index;
        if ledger_inclusion_state.is_some() {
            confirmed_milestone_index = Some(milestone_index);
        } else {
            confirmed_milestone_index = None;
        }
        if let Essence::Regular(regular) = transaction.essence() {
            for (input_index, input) in regular.inputs().iter().enumerate() {
                // insert utxoinput row along with input row
                if let Input::UTXO(utxo_input) = input {
                    let unlock_block = &unlock_blocks[input_index];
                    let input_data = InputData::utxo(utxo_input.clone(), unlock_block.clone());
                    // insert input row
                    self.insert_input(
                        message_id,
                        &transaction_id,
                        input_index as u16,
                        input_data,
                        ledger_inclusion_state,
                        confirmed_milestone_index,
                    );
                    // this is the spent_output which the input is spending from
                    let output_id = utxo_input.output_id();
                    // therefore we insert utxo_input.output_id() -> unlock_block to indicate that this output is_spent;
                    let unlock_data = UnlockData::new(transaction_id, input_index as u16, unlock_block.clone());
                    self.insert_unlock(
                        &message_id,
                        output_id.transaction_id(),
                        output_id.index(),
                        unlock_data,
                        ledger_inclusion_state,
                        confirmed_milestone_index,
                    );
                } else if let Input::Treasury(treasury_input) = input {
                    let input_data = InputData::treasury(treasury_input.clone());
                    // insert input row
                    self.insert_input(
                        message_id,
                        &transaction_id,
                        input_index as u16,
                        input_data,
                        ledger_inclusion_state,
                        confirmed_milestone_index,
                    );
                } else {
                    error!("A new input variant was added to this type!")
                }
            }
            for (output_index, output) in regular.outputs().iter().enumerate() {
                // insert output row
                self.insert_output(
                    message_id,
                    &transaction_id,
                    output_index as u16,
                    output.clone(),
                    ledger_inclusion_state,
                    confirmed_milestone_index,
                );
            }
            if let Some(payload) = regular.payload() {
                self.insert_payload(message_id, payload, milestone_index, ledger_inclusion_state)
            }
        };
    }
    fn insert_input(
        &self,
        message_id: &MessageId,
        transaction_id: &TransactionId,
        index: u16,
        input_data: InputData,
        inclusion_state: Option<LedgerInclusionState>,
        milestone_index: Option<MilestoneIndex>,
    ) {
        // -input variant: (InputTransactionId, InputIndex) -> UTXOInput data column
        let input_id = (*transaction_id, index);
        let transaction_record = TransactionRecord::input(*message_id, input_data, inclusion_state, milestone_index);
        self.insert(&self.get_keyspace(), input_id, transaction_record)
    }
    fn insert_unlock(
        &self,
        message_id: &MessageId,
        utxo_transaction_id: &TransactionId,
        utxo_index: u16,
        unlock_data: UnlockData,
        inclusion_state: Option<LedgerInclusionState>,
        milestone_index: Option<MilestoneIndex>,
    ) {
        // -unlock variant: (UtxoInputTransactionId, UtxoInputOutputIndex) -> Unlock data column
        let utxo_id = (*utxo_transaction_id, utxo_index);
        let transaction_record = TransactionRecord::unlock(*message_id, unlock_data, inclusion_state, milestone_index);
        self.insert(&self.get_keyspace(), utxo_id, transaction_record)
    }
    fn insert_output(
        &self,
        message_id: &MessageId,
        transaction_id: &TransactionId,
        index: u16,
        output: Output,
        inclusion_state: Option<LedgerInclusionState>,
        milestone_index: Option<MilestoneIndex>,
    ) {
        // -output variant: (OutputTransactionId, OutputIndex) -> Output data column
        let output_id = (*transaction_id, index);
        let transaction_record = TransactionRecord::output(*message_id, output, inclusion_state, milestone_index);
        self.insert(&self.get_keyspace(), output_id, transaction_record)
    }
    fn insert_address(
        &self,
        transaction_id: &TransactionId,
        index: u16,
        output: &Output,
        milestone_index: MilestoneIndex,
        inclusion_state: Option<LedgerInclusionState>,
    ) {
        let partition_id = self.partitioner.partition_id(milestone_index.0);
        let address_type = output.kind();
        match output {
            Output::SignatureLockedSingle(sls) => {
                let partitioned = Partitioned::new(sls.address().clone(), partition_id);
                let address_record = AddressRecord::new(
                    milestone_index,
                    *transaction_id,
                    index,
                    sls.amount(),
                    address_type,
                    inclusion_state,
                );
                // self.insert(partitioned, address_record);
            }
            Output::SignatureLockedDustAllowance(slda) => {}
            e => {
                if let Output::Treasury(_) = e {
                } else {
                    error!("Unexpected new output variant {:?}", e);
                }
            }
        }
    }
    fn insert<S, K, V>(&self, keyspace: &S, key: K, value: V)
    where
        S: 'static + Insert<K, V>,
        K: 'static + Send + Clone,
        V: 'static + Send + Clone,
    {
        let insert_req = keyspace.insert(&key, &value).consistency(Consistency::One).build();
        let worker = InsertWorker::boxed(keyspace.clone(), key, value);
        insert_req.send_local(worker);
    }
}
