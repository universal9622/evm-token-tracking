use crate::{
    address::{ERC165DerivedOrNot, IdentifiableAddress},
    prisma::{
        self,
        logs::{self, Data},
        states,
        tokens::{self},
        Chain, IndexedType, Standard,
    },
};
use ethers::{
    abi::AbiEncode,
    core::types::Filter,
    providers::{Http, Middleware, Provider},
    types::{Log, H160, H256, U64},
};
use log::error;
use prisma::PrismaClient;
use prisma_client_rust::QueryError;
use std::{process, str::FromStr, sync::Arc, time::Duration};

const EVENT_TRANSFER_ERC721: &str = "Transfer(address,address,uint256)";
const EVENT_TRANSFER_ERC1155_SINGLE: &str =
    "TransferSingle(address,address,address,uint256,uint256)";
const EVENT_TRANSFER_ERC1155_BATCH: &str =
    "TransferBatch(address,address,address,uint256[],uint256[])";

pub struct Indexer {
    pub chain: Chain,
    pub rpc_client: Arc<Provider<Http>>,
    pub db_client: PrismaClient,
}

pub async fn new(chain: Chain, url: &str) -> Indexer {
    let provider = Provider::<Http>::try_from(url).unwrap();
    let rpc_client = Arc::new(provider);
    let db_client = PrismaClient::_builder().build().await.unwrap();
    Indexer {
        chain,
        rpc_client,
        db_client,
    }
}

impl Indexer {
    async fn get_indexed_block(&self, indexed_type: IndexedType) -> i64 {
        let res = self
            .db_client
            .states()
            .find_unique(states::chain_indexed_type(self.chain, indexed_type))
            .exec()
            .await
            .unwrap();
        if let None = res {
            error!(
                "Indexed block not found for {:?} {:?}",
                self.chain, indexed_type
            );
            process::exit(1);
        }
        res.unwrap().indexed_block
    }

    pub async fn index_logs(&self) {
        let mut last_block: u64 = self.get_indexed_block(IndexedType::Log).await as u64;
        loop {
            let to_block = last_block + 100;
            let filter = Filter::new()
                .events(vec![
                    EVENT_TRANSFER_ERC721,
                    EVENT_TRANSFER_ERC1155_SINGLE,
                    EVENT_TRANSFER_ERC1155_BATCH,
                ])
                .from_block(last_block)
                .to_block(to_block);
            let res = self.rpc_client.get_logs(&filter).await;
            if let Err(e) = res {
                error!("Failed to get logs: {}", e);
                continue;
            }
            let logs = res.unwrap();
            'dumplogs: for log in logs.iter() {
                match self.dump_log(log).await {
                    Ok(_) => {
                        last_block = log.block_number.unwrap_or(U64::from(last_block)).as_u64();
                    }
                    Err(e) => {
                        error!("failed to dump log ({:?}) cause by {}", log, e);
                        break 'dumplogs;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn dump_log(&self, log: &Log) -> Result<Data, QueryError> {
        let block_number = log.block_number.unwrap().as_u64() as i64;
        let log_index = log.log_index.unwrap().encode_hex();
        let tx_hash = log.transaction_hash.unwrap().encode_hex();
        let address = log.address.encode_hex();
        let topics = log.topics.iter().map(|x| x.encode_hex()).collect();
        let data = log.data.to_vec();
        self.db_client
            .logs()
            .upsert(
                logs::block_number_log_index(block_number, log_index.to_owned()),
                logs::create(
                    tx_hash,
                    block_number,
                    log_index,
                    address,
                    data,
                    vec![logs::topics::set(topics)],
                ),
                vec![],
            )
            .exec()
            .await
    }

    pub async fn index_tokens(&self) {
        let mut indexed_block = self.get_indexed_block(IndexedType::Token).await;
        loop {
            let data = self
                .db_client
                .logs()
                .find_many(vec![logs::block_number::gte(indexed_block)])
                .take(1000)
                .exec()
                .await
                .unwrap();
            'dumptoken: for log in data {
                if let Ok(_) = self.dump_token(&log).await {
                    indexed_block = log.block_number;
                } else {
                    break 'dumptoken;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn dump_token(&self, log: &logs::Data) -> Result<tokens::Data, ()> {
        let addr = IdentifiableAddress {
            address: H160::from_str(&log.address).unwrap(),
        };
        let value = log.data.to_vec();
        let topics = &log.topics;
        let token_id = H256::from_slice(&value).encode_hex();
        let contract = &log.address;
        let to = &topics[2];

        let res = addr.check_standard(&self.rpc_client).await;
        if let Err(e) = res {
            error!("Failed to identify address: {:?}", e);
            return Err(());
        };
        let standard;
        match res.unwrap() {
            ERC165DerivedOrNot::OTHER => return Err(()),
            ERC165DerivedOrNot::ERC1155 => standard = Standard::Erc1155,
            ERC165DerivedOrNot::ERC721 => standard = Standard::Erc721,
        };
        let res = self
            .db_client
            .tokens()
            .upsert(
                tokens::chain_token_id_contract(
                    self.chain,
                    token_id.to_owned(),
                    contract.to_owned(),
                ),
                tokens::create(
                    self.chain,
                    token_id,
                    contract.to_owned(),
                    to.to_owned(),
                    standard,
                    vec![],
                ),
                vec![],
            )
            .exec()
            .await;
        if let Err(e) = res {
            error!("Failed to upsert data: {}", e);
            return Err(());
        }
        Ok(res.unwrap())
    }
}
