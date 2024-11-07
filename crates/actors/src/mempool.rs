use actix::{Actor, Context, Handler, Message};
use irys_types::{
    app_state::DatabaseProvider, chunk::Chunk, hash_sha256, validate_path, IrysTransactionHeader,
    CHUNK_SIZE, H256,
};
use reth_db::DatabaseEnv;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolActor {
    db: DatabaseProvider,
    /// Temporary mempool stubs - will replace with proper data models - dmac
    valid_tx: BTreeMap<H256, IrysTransactionHeader>,
    invalid_tx: Vec<H256>,
}

impl Actor for MempoolActor {
    type Context = Context<Self>;
}

impl MempoolActor {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a DatabaseEnv
    pub fn new(db: DatabaseProvider) -> Self {
        Self {
            db,
            valid_tx: BTreeMap::new(),
            invalid_tx: Vec::new(),
        }
    }
}

/// Message for when a new TX is discovered by the node, either though
/// synchronization with peers, or by a user posting the tx.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),TxIngressError>")]
pub struct TxIngressMessage(pub IrysTransactionHeader);

impl TxIngressMessage {
    fn into_inner(self) -> IrysTransactionHeader {
        self.0
    }
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug)]
pub enum TxIngressError {
    /// The transaction's signature is invalid
    InvalidSignature,
    /// The account does not have enough tokens to fund this transaction
    Unfunded,
    /// This transaction id is already in the cache
    Skipped,
}

/// Message for when a new chunk is discovered by the node, either though
/// synchronization with peers, or by a user posting the chunk.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),ChunkIngressError>")]
pub struct ChunkIngressMessage(pub Chunk);

impl ChunkIngressMessage {
    fn into_inner(self) -> Chunk {
        self.0
    }
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug)]
pub enum ChunkIngressError {
    /// The data_path/proof provided with the chunk data is invalid
    InvalidProof,
    /// The data hash does not match the chunk data
    InvalidDataHash,
    /// Only the last chunk in a data_root tree can be less than CHUNK_SIZE
    InvalidChunkSize,
    /// Some database error occurred when reading or writing the chunk
    DatabaseError,
}

impl Handler<TxIngressMessage> for MempoolActor {
    type Result = Result<(), TxIngressError>;

    fn handle(&mut self, tx_msg: TxIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let tx = &tx_msg.0;

        // Early out if we already know about this transaction
        if self.invalid_tx.contains(&tx.id) || self.valid_tx.contains_key(&tx.id) {
            // Skip tx reprocessing if already verified (valid or invalid) to prevent
            // CPU-intensive signature verification spam attacks
            return Err(TxIngressError::Skipped);
        }

        // Validate the transaction signature
        if tx.is_signature_valid() {
            println!("Signature is valid");
            self.valid_tx.insert(tx.id, tx.clone());
        } else {
            self.invalid_tx.push(tx.id);
            println!("Signature is NOT valid");
            return Err(TxIngressError::InvalidSignature);
        }

        // TODO: Check if the signer has funds to post the tx
        //return Err(TxIngressError::Unfunded);

        // Cache the data_root in the database
        let _ = database::cache_data_root(&self.db, &tx);

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolActor {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, chunk_msg: ChunkIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let chunk = chunk_msg.0;
        // Check to see if we have a cached data_root for this chunk
        let result = database::cached_data_root_by_data_root(&self.db, chunk.data_root);

        let cached_data_root = result
            .map_err(|_| ChunkIngressError::DatabaseError)? // Convert DatabaseError to ChunkIngressError
            .ok_or(ChunkIngressError::InvalidDataHash)?; // Handle None case by converting it to an error

        // Next validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let root_hash = chunk.data_root.0;
        let target_offset = chunk.offset as u128;
        let path_buff = &chunk.data_path;

        let path_result = match validate_path(root_hash, path_buff, target_offset) {
            Ok(result) => result,
            Err(_) => {
                return Err(ChunkIngressError::InvalidProof);
            }
        };

        // Validate that the data_size for this chunk matches the data_size
        // recorded in the transaction header.
        if cached_data_root.data_size != chunk.data_size {
            return Err(ChunkIngressError::InvalidDataHash);
        }

        // Use that data_Size to identify  and validate that only the last chunk
        // can be less than 256KB
        let chunk_len = chunk.bytes.len() as u64;
        if (chunk.offset as u64) < chunk.data_size - 1 {
            // Ensure prefix chunks are all exactly CHUNK_SIZE
            if chunk_len != CHUNK_SIZE {
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        } else {
            // Ensure the last chunk is no larger than CHUNK_SIZE
            if chunk_len > CHUNK_SIZE {
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        }

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size

        // Check that the leaf hash on the data_path matches the chunk_hash
        if path_result.leaf_hash == hash_sha256(&chunk.bytes.0).unwrap() {
            // Finally write the chunk to CachedChunks
            let _ = database::cache_chunk(&self.db, chunk);
            Ok(())
        } else {
            Err(ChunkIngressError::InvalidDataHash)
        }
    }
}

// Message for getting txs for block building
#[derive(Message, Debug)]
#[rtype(result = "Vec<IrysTransactionHeader>")]
pub struct GetBestMempoolTxs;

impl Handler<GetBestMempoolTxs> for MempoolActor {
    type Result = Vec<IrysTransactionHeader>;

    fn handle(&mut self, msg: GetBestMempoolTxs, ctx: &mut Self::Context) -> Self::Result {
        vec![]
    }
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use database::{config::get_data_dir, open_or_create_db};
    use irys_types::{irys::Irys, Base64, MAX_CHUNK_SIZE};
    use rand::Rng;

    use super::*;

    use actix::prelude::*;

    #[actix::test]
    async fn post_transaction_and_chunks() {
        // Connect to the db
        let path = get_data_dir();
        let db = open_or_create_db(path).unwrap();
        let arc_db1 = DatabaseProvider(Arc::new(db));
        let arc_db2 = DatabaseProvider(Arc::clone(&arc_db1));

        // Create an instance of the mempool actor
        let mempool = MempoolActor::new(arc_db1);
        let addr: Addr<MempoolActor> = mempool.start();

        // Create 2.5 chunks worth of data *  fill the data with random bytes
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys API instance & a signed transaction
        let irys = Irys::random_signer();
        let tx = irys
            .create_transaction(data_bytes.clone(), None)
            .await
            .unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        println!("{:?}", tx.header);
        println!("{}", serde_json::to_string_pretty(&tx.header).unwrap());

        // Wrap the transaction in a TxIngressMessage
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let tx_ingress_msg = TxIngressMessage { 0: tx.header };

        // Post the TxIngressMessage to the handle method on the mempool actor
        let result = addr.send(tx_ingress_msg).await.unwrap();

        // Verify the transaction was added
        assert_matches!(result, Ok(()));

        // Verify the data_root was added to the cache
        let result = database::cached_data_root_by_data_root(&arc_db2, data_root).unwrap();
        assert_matches!(result, Some(_));

        // Loop though each of the transaction chunks
        for (index, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let offset = tx.proofs[index].offset;
            let data_path = Base64(tx.proofs[index].proof.to_vec());
            let key: H256 = hash_sha256(&data_path.0).unwrap().into();

            // Create a ChunkIngressMessage for each chunk
            let chunk_ingress_msg = ChunkIngressMessage {
                0: Chunk {
                    data_root,
                    data_size,
                    data_path,
                    bytes: Base64(data_bytes[min..max].to_vec()),
                    offset,
                },
            };

            // Post the ChunkIngressMessage to the handle method on the mempool
            let result = addr.send(chunk_ingress_msg).await.unwrap();

            // Verify the chunk was added
            assert_matches!(result, Ok(()));

            // Verify the chunk is added to the ChunksCache
            let result = database::cached_chunk_by_chunk_key(&arc_db2, key).unwrap();
            assert_matches!(result, Some(_));
        }

        // Modify one of the chunks

        // Attempt to post the chunk

        // Verify there chunk is not accepted
    }
}
