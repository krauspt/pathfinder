use crate::context::RpcContext;
use crate::v02::types::reply::BlockStatus;

use anyhow::{anyhow, Context};
use pathfinder_common::BlockId;
use serde::Deserialize;

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Copy, Clone))]
#[serde(deny_unknown_fields)]
pub struct GetBlockInput {
    block_id: BlockId,
}

crate::error::generate_rpc_error_subset!(GetBlockError: BlockNotFound);

/// Get block information with transaction hashes given the block id
pub async fn get_block_with_tx_hashes(
    context: RpcContext,
    input: GetBlockInput,
) -> Result<types::Block, GetBlockError> {
    let block_id = input.block_id;
    let block_id = match block_id {
        BlockId::Pending => {
            match context
                .pending_data
                .ok_or_else(|| anyhow!("Pending data not supported in this configuration"))?
                .block()
                .await
            {
                Some(block) => {
                    return Ok(types::Block::from_sequencer(block.as_ref().clone().into()))
                }
                None => return Err(GetBlockError::BlockNotFound),
            }
        }
        other => other.try_into().expect("Only pending cast should fail"),
    };

    let storage = context.storage.clone();
    let span = tracing::Span::current();

    tokio::task::spawn_blocking(move || {
        let _g = span.enter();
        let mut connection = storage
            .connection()
            .context("Opening database connection")?;

        let transaction = connection
            .transaction()
            .context("Creating database transaction")?;

        let header = transaction
            .block_header(block_id)
            .context("Reading block from database")?
            .ok_or(GetBlockError::BlockNotFound)?;

        let l1_accepted = transaction.block_is_l1_accepted(header.number.into())?;
        let block_status = if l1_accepted {
            BlockStatus::AcceptedOnL1
        } else {
            BlockStatus::AcceptedOnL2
        };

        let transactions = transaction
            .transaction_hashes_for_block(header.number.into())
            .context("Reading transaction hashes")?
            .context("Missing block")?;

        Ok(types::Block::from_parts(header, block_status, transactions))
    })
    .await
    .context("Database read panic or shutting down")?
}

mod types {
    use crate::v02::types::reply::BlockStatus;
    use pathfinder_common::{BlockHeader, TransactionHash};
    use serde::Serialize;

    /// L2 Block as returned by the RPC API.
    #[derive(Clone, Debug, Serialize, PartialEq, Eq)]
    pub struct Block {
        #[serde(flatten)]
        pub header: crate::v05::types::BlockHeader,
        pub status: BlockStatus,
        pub transactions: Vec<TransactionHash>,
    }

    impl Block {
        pub fn from_parts(
            header: BlockHeader,
            status: BlockStatus,
            transactions: Vec<TransactionHash>,
        ) -> Self {
            Self {
                header: header.into(),
                status,
                transactions,
            }
        }

        /// Constructs [Block] from [sequencer's block representation](starknet_gateway_types::reply::Block)
        pub fn from_sequencer(block: starknet_gateway_types::reply::MaybePendingBlock) -> Self {
            Self {
                status: block.status().into(),
                transactions: block.transactions().iter().map(|t| t.hash()).collect(),
                header: crate::v05::types::BlockHeader::from_sequencer(block),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use pathfinder_common::macro_prelude::*;
    use pathfinder_common::BlockNumber;
    use serde_json::json;
    use starknet_gateway_types::pending::PendingData;

    #[rstest::rstest]
    #[case::pending_by_position(json!(["pending"]), BlockId::Pending)]
    #[case::pending_by_name(json!({"block_id": "pending"}), BlockId::Pending)]
    #[case::latest_by_position(json!(["latest"]), BlockId::Latest)]
    #[case::latest_by_name(json!({"block_id": "latest"}), BlockId::Latest)]
    #[case::number_by_position(json!([{"block_number":123}]), BlockNumber::new_or_panic(123).into())]
    #[case::number_by_name(json!({"block_id": {"block_number":123}}), BlockNumber::new_or_panic(123).into())]
    #[case::hash_by_position(json!([{"block_hash": "0xbeef"}]), block_hash!("0xbeef").into())]
    #[case::hash_by_name(json!({"block_id": {"block_hash": "0xbeef"}}), block_hash!("0xbeef").into())]
    fn input_parsing(#[case] input: serde_json::Value, #[case] block_id: BlockId) {
        let input = serde_json::from_value::<GetBlockInput>(input).unwrap();

        let expected = GetBlockInput { block_id };

        assert_eq!(input, expected);
    }

    type TestCaseHandler = Box<dyn Fn(usize, &Result<types::Block, GetBlockError>)>;

    /// Execute a single test case and check its outcome for both: `get_block_with_[txs|tx_hashes]`
    async fn check(test_case_idx: usize, test_case: &(RpcContext, BlockId, TestCaseHandler)) {
        let (context, block_id, f) = test_case;
        let result = get_block_with_tx_hashes(
            context.clone(),
            GetBlockInput {
                block_id: *block_id,
            },
        )
        .await;
        f(test_case_idx, &result);
    }

    /// Common assertion type for most of the test cases
    fn assert_hash(expected: &'static [u8]) -> TestCaseHandler {
        Box::new(|i: usize, result| {
            assert_matches!(result, Ok(block) => assert_eq!(
                block.header.block_hash,
                Some(block_hash_bytes!(expected)),
                "test case {i}"
            ));
        })
    }

    impl PartialEq for GetBlockError {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Internal(l), Self::Internal(r)) => l.to_string() == r.to_string(),
                _ => core::mem::discriminant(self) == core::mem::discriminant(other),
            }
        }
    }

    /// Common assertion type for most of the error paths
    fn assert_error(expected: GetBlockError) -> TestCaseHandler {
        Box::new(move |i: usize, result| {
            assert_matches!(result, Err(error) => assert_eq!(*error, expected, "test case {i}"), "test case {i}");
        })
    }

    #[tokio::test]
    async fn happy_paths_and_major_errors() {
        let ctx: RpcContext = RpcContext::for_tests_with_pending().await;
        let ctx_with_pending_empty =
            RpcContext::for_tests().with_pending_data(PendingData::default());
        let ctx_with_pending_disabled = RpcContext::for_tests();

        let cases: &[(RpcContext, BlockId, TestCaseHandler)] = &[
            // Pending
            (
                ctx.clone(),
                BlockId::Pending,
                Box::new(|i, result| {
                    assert_matches!(result, Ok(block) => assert_eq!(
                        block.header.parent_hash,
                        block_hash_bytes!(b"latest"),
                        "test case {i}"
                    ), "test case {i}")
                }),
            ),
            (
                ctx_with_pending_empty,
                BlockId::Pending,
                assert_error(GetBlockError::BlockNotFound),
            ),
            (
                ctx_with_pending_disabled,
                BlockId::Pending,
                assert_error(GetBlockError::Internal(anyhow!(
                    "Pending data not supported in this configuration"
                ))),
            ),
            // Other block ids
            (ctx.clone(), BlockId::Latest, assert_hash(b"latest")),
            (
                ctx.clone(),
                BlockId::Number(BlockNumber::GENESIS),
                assert_hash(b"genesis"),
            ),
            (
                ctx.clone(),
                BlockId::Hash(block_hash_bytes!(b"genesis")),
                assert_hash(b"genesis"),
            ),
            (
                ctx.clone(),
                BlockId::Number(BlockNumber::new_or_panic(9999)),
                assert_error(GetBlockError::BlockNotFound),
            ),
            (
                ctx,
                BlockId::Hash(block_hash_bytes!(b"non-existent")),
                assert_error(GetBlockError::BlockNotFound),
            ),
        ];

        for (i, test_case) in cases.iter().enumerate() {
            check(i, test_case).await;
        }
    }

    #[tokio::test]
    async fn transaction_hashes_are_correct() {
        let ctx: RpcContext = RpcContext::for_tests();

        let tx_hashes = get_block_with_tx_hashes(
            ctx,
            GetBlockInput {
                block_id: BlockNumber::new_or_panic(1).into(),
            },
        )
        .await
        .unwrap()
        .transactions;

        assert_eq!(
            tx_hashes,
            vec![
                transaction_hash_bytes!(b"txn 1"),
                transaction_hash_bytes!(b"txn 2")
            ]
        );
    }
}