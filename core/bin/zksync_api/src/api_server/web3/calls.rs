// Built-in uses
use std::collections::HashMap;
use std::convert::TryInto;
use std::path::PathBuf;
use std::str::FromStr;
// External uses
use ethabi::{encode, Contract, Function, Token as AbiToken};
use jsonrpc_core::{Error, Result};
use tiny_keccak::keccak256;
// Workspace uses
use zksync_storage::StorageProcessor;
use zksync_types::{TokenId, NFT};
// Local uses
use super::{
    converter::u256_from_biguint,
    types::{H160, U256},
    ZKSYNC_PROXY_ADDRESS,
};
use crate::utils::token_db_cache::TokenDBCache;

#[derive(Debug, Clone)]
pub struct CallsHelper {
    erc20: HashMap<[u8; 4], Function>,
    zksync_proxy: HashMap<[u8; 4], Function>,
    tokens: TokenDBCache,
    zksync_proxy_address: H160,
}

impl CallsHelper {
    const SHA256_MULTI_HASH: [u8; 2] = [18, 32]; // 0x1220
    const ALPHABET: &'static str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    fn gen_hashmap(functions: Vec<Function>) -> HashMap<[u8; 4], Function> {
        functions
            .into_iter()
            .map(|f| {
                let inputs = f
                    .inputs
                    .iter()
                    .map(|p| p.kind.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let signature = format!("{}({})", f.name, inputs);
                let selector: [u8; 4] = keccak256(signature.as_bytes())[0..4].try_into().unwrap();
                (selector, f)
            })
            .collect()
    }

    pub fn new() -> Self {
        let mut path = PathBuf::new();
        path.push(std::env::var("ZKSYNC_HOME").unwrap_or_else(|_| "/".to_string()));
        path.push("core/bin/zksync_api/src/api_server/web3/abi");
        let erc20_abi = std::fs::File::open(path.join("ERC20.json")).unwrap();
        let erc20_functions = Contract::load(erc20_abi)
            .unwrap()
            .functions
            .values()
            .flatten()
            .cloned()
            .collect();
        let erc20_function_by_selector = Self::gen_hashmap(erc20_functions);

        let zksync_proxy_abi = std::fs::File::open(path.join("ZkSyncProxy.json")).unwrap();
        let zksync_proxy_functions = Contract::load(zksync_proxy_abi)
            .unwrap()
            .functions
            .values()
            .flatten()
            .cloned()
            .collect();
        let zksync_proxy_function_by_selector = Self::gen_hashmap(zksync_proxy_functions);

        Self {
            erc20: erc20_function_by_selector,
            zksync_proxy: zksync_proxy_function_by_selector,
            tokens: TokenDBCache::new(),
            zksync_proxy_address: H160::from_str(ZKSYNC_PROXY_ADDRESS).unwrap(),
        }
    }

    pub async fn execute(
        &self,
        storage: &mut StorageProcessor<'_>,
        to: H160,
        data: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let all_functions = if to == self.zksync_proxy_address {
            &self.zksync_proxy
        } else {
            let token = self
                .tokens
                .get_token(storage, to)
                .await
                .map_err(|_| Error::internal_error())?;
            if let Some(token) = token {
                if !token.is_nft {
                    &self.erc20
                } else {
                    return Ok(Vec::new());
                }
            } else {
                return Ok(Vec::new());
            }
        };
        let selector: [u8; 4] = if data.len() >= 4 {
            data[0..4].try_into().unwrap()
        } else {
            return Ok(Vec::new());
        };
        let function = if let Some(function) = all_functions.get(&selector) {
            function
        } else {
            return Ok(Vec::new());
        };
        let params = if let Ok(params) = function.decode_input(&data[4..]) {
            params
        } else {
            return Ok(Vec::new());
        };

        let result = if to == self.zksync_proxy_address {
            match function.name.as_str() {
                "creatorId" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if let Some(nft) = self.get_nft(storage, token_id).await? {
                        encode(&[AbiToken::Uint(U256::from(nft.creator_id.0))])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                "creatorAddress" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if let Some(nft) = self.get_nft(storage, token_id).await? {
                        encode(&[AbiToken::Address(nft.creator_address)])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                "serialId" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if let Some(nft) = self.get_nft(storage, token_id).await? {
                        encode(&[AbiToken::Uint(U256::from(nft.serial_id))])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                "contentHash" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if let Some(nft) = self.get_nft(storage, token_id).await? {
                        encode(&[AbiToken::FixedBytes(nft.content_hash.as_bytes().to_vec())])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                "tokenURI" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if let Some(nft) = self.get_nft(storage, token_id).await? {
                        let ipfs_cid = Self::ipfs_cid(nft.content_hash.as_bytes());
                        encode(&[AbiToken::String(format!("ipfs://{}", ipfs_cid))])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                "balanceOf" => {
                    let address = params[0]
                        .clone()
                        .into_address()
                        .ok_or_else(Error::internal_error)?;
                    let balance = storage
                        .chain()
                        .account_schema()
                        .get_account_nft_balance(address)
                        .await
                        .map_err(|_| Error::internal_error())?;
                    encode(&[AbiToken::Uint(U256::from(balance))])
                }
                "ownerOf" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if let Some(nft) = self.get_nft(storage, token_id).await? {
                        let owner = storage
                            .chain()
                            .account_schema()
                            .get_nft_owner(nft.id)
                            .await
                            .map_err(|_| Error::internal_error())?;
                        encode(&[AbiToken::Address(owner)])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                "getApproved" => {
                    let token_id = params[0]
                        .clone()
                        .into_uint()
                        .ok_or_else(Error::internal_error)?;
                    if self.get_nft(storage, token_id).await?.is_some() {
                        encode(&[AbiToken::Address(self.zksync_proxy_address)])
                    } else {
                        return Ok(Vec::new());
                    }
                }
                _ => unreachable!(),
            }
        } else {
            let token = self
                .tokens
                .get_token(storage, to)
                .await
                .map_err(|_| Error::internal_error())?
                .ok_or_else(Error::internal_error)?;
            match function.name.as_str() {
                "name" | "symbol" => encode(&[AbiToken::String(token.symbol)]),
                "decimals" => encode(&[AbiToken::Uint(U256::from(token.decimals))]),
                "totalSupply" | "allowance" => encode(&[AbiToken::Uint(U256::max_value())]),
                "balanceOf" => {
                    let block = storage
                        .chain()
                        .block_schema()
                        .get_last_saved_block()
                        .await
                        .map_err(|_| Error::internal_error())?;
                    let address = params[0]
                        .clone()
                        .into_address()
                        .ok_or_else(Error::internal_error)?;
                    let balance = storage
                        .chain()
                        .account_schema()
                        .get_account_balance_for_block(address, block, token.id)
                        .await
                        .map_err(|_| Error::internal_error())?;
                    encode(&[AbiToken::Uint(u256_from_biguint(balance)?)])
                }
                _ => unreachable!(),
            }
        };
        Ok(result)
    }

    async fn get_nft(
        &self,
        storage: &mut StorageProcessor<'_>,
        token_id: U256,
    ) -> Result<Option<NFT>> {
        if token_id > U256::from(u32::MAX) {
            return Ok(None);
        }
        let nft = self
            .tokens
            .get_nft_by_id(storage, TokenId(token_id.as_u32()))
            .await
            .map_err(|_| Error::internal_error())?;
        Ok(nft)
    }

    fn to_base58(source: &[u8]) -> String {
        let mut digits: [u8; 46] = [0; 46];
        let mut digit_length: usize = 1;
        for mut carry in source.iter().map(|a| *a as u32) {
            for j in 0..digit_length {
                carry += (digits[j] as u32) * 256;
                digits[j] = (carry % 58) as u8;
                carry /= 58;
            }

            while carry > 0 {
                digits[digit_length] = (carry % 58) as u8;
                digit_length += 1;
                carry /= 58;
            }
        }

        let result: Vec<u8> = digits.iter().rev().copied().collect();
        Self::to_alphabet(&result)
    }

    fn ipfs_cid(source: &[u8]) -> String {
        let concat: Vec<u8> = Self::SHA256_MULTI_HASH
            .iter()
            .chain(source.iter())
            .copied()
            .collect();
        Self::to_base58(&concat)
    }

    fn to_alphabet(indices: &[u8]) -> String {
        let mut output = String::new();
        for i in indices {
            output.push(Self::ALPHABET.as_bytes()[*i as usize] as char)
        }
        return output;
    }
}
