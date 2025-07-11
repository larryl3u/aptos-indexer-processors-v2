// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

// TODO: add back all models and configs back as we migrate
use crate::{
    parquet_processors::{
        parquet_ans::parquet_ans_processor::ParquetAnsProcessorConfig,
        parquet_transaction_metadata::transaction_metadata_models::write_set_size_info::ParquetWriteSetSize,
        parquet_utils::util::{format_table_name, NamedTable, VALID_TABLE_NAMES},
    },
    processors::{
        account_transactions::account_transactions_model::ParquetAccountTransaction,
        ans::{
            ans_processor::AnsProcessorConfig,
            models::{
                ans_lookup_v2::{ParquetAnsLookupV2, ParquetCurrentAnsLookupV2},
                ans_primary_name_v2::{ParquetAnsPrimaryNameV2, ParquetCurrentAnsPrimaryNameV2},
            },
        },
        default::models::{
            block_metadata_transactions::ParquetBlockMetadataTransaction,
            move_modules::ParquetMoveModule,
            move_resources::ParquetMoveResource,
            table_items::{ParquetCurrentTableItem, ParquetTableItem, ParquetTableMetadata},
            transactions::ParquetTransaction,
            write_set_changes::ParquetWriteSetChange,
        },
        events::events_model::ParquetEvent,
        fungible_asset::fungible_asset_models::{
            v2_fungible_asset_activities::ParquetFungibleAssetActivity,
            v2_fungible_asset_balances::ParquetFungibleAssetBalance,
            v2_fungible_asset_to_coin_mappings::ParquetFungibleAssetToCoinMapping,
            v2_fungible_metadata::ParquetFungibleAssetMetadataModel,
        },
        objects::{
            objects_processor::ObjectsProcessorConfig,
            v2_objects_models::{ParquetCurrentObject, ParquetObject},
        },
        stake::{
            models::{
                delegator_activities::ParquetDelegatedStakingActivity,
                delegator_balances::{ParquetCurrentDelegatorBalance, ParquetDelegatorBalance},
                proposal_votes::ParquetProposalVote,
            },
            stake_processor::StakeProcessorConfig,
        },
        token_v2::{
            token_models::{
                token_claims::ParquetCurrentTokenPendingClaim,
                token_royalty::ParquetCurrentTokenRoyaltyV1,
            },
            token_v2_models::{
                v2_token_activities::ParquetTokenActivityV2,
                v2_token_datas::{ParquetCurrentTokenDataV2, ParquetTokenDataV2},
                v2_token_metadata::ParquetCurrentTokenV2Metadata,
                v2_token_ownerships::{ParquetCurrentTokenOwnershipV2, ParquetTokenOwnershipV2},
            },
            token_v2_processor::TokenV2ProcessorConfig,
        },
        user_transaction::models::user_transactions::ParquetUserTransaction,
    },
};
use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// This enum captures the configs for all the different processors that are defined.
///
/// The configs for each processor should only contain configuration specific to that
/// processor. For configuration that is common to all processors, put it in
/// IndexerGrpcProcessorConfig.
#[derive(Clone, Debug, Deserialize, Serialize, strum::IntoStaticStr, strum::EnumDiscriminants)]
#[serde(tag = "type", rename_all = "snake_case")]
// What is all this strum stuff? Let me explain.
//
// Previously we had consts called NAME in each module and a function called `name` on
// the ProcessorTrait. As such it was possible for this name to not match the snake case
// representation of the struct name. By using strum we can have a single source for
// processor names derived from the enum variants themselves.
//
// That's what this strum_discriminants stuff is, it uses macro magic to generate the
// ProcessorName enum based on ProcessorConfig. The rest of the derives configure this
// generation logic, e.g. to make sure we use snake_case.
#[strum(serialize_all = "snake_case")]
#[strum_discriminants(
    derive(
        Deserialize,
        Serialize,
        strum::EnumVariantNames,
        strum::IntoStaticStr,
        strum::Display,
        clap::ValueEnum,
        strum::EnumIter
    ),
    name(ProcessorName),
    clap(rename_all = "snake_case"),
    serde(rename_all = "snake_case"),
    strum(serialize_all = "snake_case")
)]
pub enum ProcessorConfig {
    AccountRestorationProcessor(DefaultProcessorConfig),
    AccountTransactionsProcessor(DefaultProcessorConfig),
    AnsProcessor(AnsProcessorConfig),
    DefaultProcessor(DefaultProcessorConfig),
    EventsProcessor(DefaultProcessorConfig),
    FungibleAssetProcessor(DefaultProcessorConfig),
    UserTransactionProcessor(DefaultProcessorConfig),
    StakeProcessor(StakeProcessorConfig),
    TokenV2Processor(TokenV2ProcessorConfig),
    ObjectsProcessor(ObjectsProcessorConfig),
    MonitoringProcessor(DefaultProcessorConfig),
    GasFeeProcessor(DefaultProcessorConfig),
    // ParquetProcessor
    ParquetDefaultProcessor(ParquetDefaultProcessorConfig),
    ParquetObjectsProcessor(ParquetDefaultProcessorConfig),
    ParquetUserTransactionProcessor(ParquetDefaultProcessorConfig),
    ParquetEventsProcessor(ParquetDefaultProcessorConfig),
    ParquetAnsProcessor(ParquetAnsProcessorConfig),
    ParquetFungibleAssetProcessor(ParquetDefaultProcessorConfig),
    ParquetTransactionMetadataProcessor(ParquetDefaultProcessorConfig),
    ParquetAccountTransactionsProcessor(ParquetDefaultProcessorConfig),
    ParquetTokenV2Processor(ParquetDefaultProcessorConfig),
    ParquetStakeProcessor(ParquetDefaultProcessorConfig),
}

impl ProcessorConfig {
    /// Get the name of the processor config as a static str. This is a convenience
    /// method to access the derived functionality implemented by strum::IntoStaticStr.
    pub fn name(&self) -> &'static str {
        self.into()
    }

    // TODO: uncomment after we migrate all parquet processors
    /// Get the Vec of table names for parquet processors only.
    ///
    /// This is a convenience method to map the table names to include the processor name as a prefix, which
    /// is useful for querying the status from the processor status table in the database.
    pub fn get_processor_status_table_names(&self) -> anyhow::Result<Vec<String>> {
        let default_config = match self {
            ProcessorConfig::ParquetDefaultProcessor(config)
            | ProcessorConfig::ParquetEventsProcessor(config)
            | ProcessorConfig::ParquetTransactionMetadataProcessor(config)
            | ProcessorConfig::ParquetAccountTransactionsProcessor(config)
            | ProcessorConfig::ParquetTokenV2Processor(config)
            | ProcessorConfig::ParquetStakeProcessor(config)
            | ProcessorConfig::ParquetObjectsProcessor(config)
            | ProcessorConfig::ParquetFungibleAssetProcessor(config)
            | ProcessorConfig::ParquetUserTransactionProcessor(config) => config,
            ProcessorConfig::ParquetAnsProcessor(config) => &config.default,
            _ => {
                return Err(anyhow::anyhow!(
                    "Invalid parquet processor config: {:?}",
                    self
                ))
            },
        };

        // Get the processor name as a prefix
        let processor_name = self.name();

        let valid_table_names = VALID_TABLE_NAMES
            .get(processor_name)
            .ok_or_else(|| anyhow::anyhow!("Processor type not recognized"))?;

        // Use the helper function for validation and mapping
        if default_config.backfill_table.is_empty() {
            Ok(valid_table_names
                .iter()
                .cloned()
                .map(|table_name| format_table_name(processor_name, &table_name))
                .collect())
        } else {
            Self::validate_backfill_table_names(&default_config.backfill_table, valid_table_names)
        }
    }

    // TODO: uncomment this when we migrate the parquet processor
    // Get the set of table names to process for the given processor.
    pub fn table_names(processor: &ProcessorName) -> HashSet<String> {
        match processor {
            ProcessorName::ParquetDefaultProcessor => HashSet::from([
                ParquetTransaction::TABLE_NAME.to_string(),
                ParquetMoveResource::TABLE_NAME.to_string(),
                ParquetWriteSetChange::TABLE_NAME.to_string(),
                ParquetTableItem::TABLE_NAME.to_string(),
                ParquetMoveModule::TABLE_NAME.to_string(),
                ParquetBlockMetadataTransaction::TABLE_NAME.to_string(),
                ParquetCurrentTableItem::TABLE_NAME.to_string(),
                ParquetTableMetadata::TABLE_NAME.to_string(),
            ]),
            ProcessorName::ParquetUserTransactionProcessor => {
                HashSet::from([ParquetUserTransaction::TABLE_NAME.to_string()])
            },
            ProcessorName::ParquetEventsProcessor => {
                HashSet::from([ParquetEvent::TABLE_NAME.to_string()])
            },
            ProcessorName::ParquetAnsProcessor => HashSet::from([
                ParquetAnsLookupV2::TABLE_NAME.to_string(),
                ParquetAnsPrimaryNameV2::TABLE_NAME.to_string(),
                ParquetCurrentAnsLookupV2::TABLE_NAME.to_string(),
                ParquetCurrentAnsPrimaryNameV2::TABLE_NAME.to_string(),
            ]),
            ProcessorName::ParquetFungibleAssetProcessor => HashSet::from([
                ParquetFungibleAssetActivity::TABLE_NAME.to_string(),
                ParquetFungibleAssetBalance::TABLE_NAME.to_string(),
                ParquetFungibleAssetMetadataModel::TABLE_NAME.to_string(),
                ParquetFungibleAssetToCoinMapping::TABLE_NAME.to_string(),
            ]),
            ProcessorName::ParquetTransactionMetadataProcessor => {
                HashSet::from([ParquetWriteSetSize::TABLE_NAME.to_string()])
            },
            ProcessorName::ParquetAccountTransactionsProcessor => {
                HashSet::from([ParquetAccountTransaction::TABLE_NAME.to_string()])
            },
            ProcessorName::ParquetTokenV2Processor => HashSet::from([
                ParquetCurrentTokenPendingClaim::TABLE_NAME.to_string(),
                ParquetCurrentTokenRoyaltyV1::TABLE_NAME.to_string(),
                ParquetCurrentTokenV2Metadata::TABLE_NAME.to_string(),
                ParquetTokenActivityV2::TABLE_NAME.to_string(),
                ParquetTokenDataV2::TABLE_NAME.to_string(),
                ParquetCurrentTokenDataV2::TABLE_NAME.to_string(),
                ParquetTokenOwnershipV2::TABLE_NAME.to_string(),
                ParquetCurrentTokenOwnershipV2::TABLE_NAME.to_string(),
            ]),
            ProcessorName::ParquetObjectsProcessor => HashSet::from([
                ParquetObject::TABLE_NAME.to_string(),
                ParquetCurrentObject::TABLE_NAME.to_string(),
            ]),
            ProcessorName::ParquetStakeProcessor => HashSet::from([
                ParquetDelegatedStakingActivity::TABLE_NAME.to_string(),
                ParquetProposalVote::TABLE_NAME.to_string(),
                ParquetDelegatorBalance::TABLE_NAME.to_string(),
                ParquetCurrentDelegatorBalance::TABLE_NAME.to_string(),
            ]),
            _ => HashSet::new(), // Default case for unsupported processors
        }
    }

    /// This is to validate table_name for the backfill table
    fn validate_backfill_table_names(
        table_names: &HashSet<String>,
        valid_table_names: &HashSet<String>,
    ) -> anyhow::Result<Vec<String>> {
        table_names
            .iter()
            .map(|table_name| {
                if !valid_table_names.contains(&table_name.to_lowercase()) {
                    return Err(anyhow::anyhow!(
                        "Invalid table name '{}'. Expected one of: {:?}",
                        table_name,
                        valid_table_names
                    ));
                }
                Ok(table_name.clone())
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultProcessorConfig {
    // Number of rows to insert, per chunk, for each DB table. Default per table is ~32,768 (2**16/2)
    #[serde(default = "AHashMap::new")]
    pub per_table_chunk_sizes: AHashMap<String, usize>,
    // Size of channel between steps
    #[serde(default = "DefaultProcessorConfig::default_channel_size")]
    pub channel_size: usize,
    // String vector for tables to write to DB, by default all tables are written
    #[serde(default)]
    pub tables_to_write: HashSet<String>,
}

impl DefaultProcessorConfig {
    pub const fn default_channel_size() -> usize {
        10
    }
}

impl Default for DefaultProcessorConfig {
    fn default() -> Self {
        Self {
            per_table_chunk_sizes: AHashMap::new(),
            channel_size: Self::default_channel_size(),
            tables_to_write: HashSet::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ParquetDefaultProcessorConfig {
    #[serde(default = "ParquetDefaultProcessorConfig::default_channel_size")]
    pub channel_size: usize,
    #[serde(default = "ParquetDefaultProcessorConfig::default_max_buffer_size")]
    pub max_buffer_size: usize,
    #[serde(default = "ParquetDefaultProcessorConfig::default_parquet_upload_interval")]
    pub upload_interval: u64,
    // Set of table name to backfill. Using HashSet for fast lookups, and for future extensibility.
    #[serde(default)]
    pub backfill_table: HashSet<String>,
}

impl ParquetDefaultProcessorConfig {
    /// Make the default very large on purpose so that by default it's not chunked
    /// This prevents any unexpected changes in behavior
    pub const fn default_channel_size() -> usize {
        100_000
    }

    /// Default maximum buffer size for parquet files in bytes
    pub const fn default_max_buffer_size() -> usize {
        1024 * 1024 * 100 // 100 MB
    }

    /// Default upload interval for parquet files in seconds
    pub const fn default_parquet_upload_interval() -> u64 {
        1800 // 30 minutes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_table_names() {
        let config = ProcessorConfig::ParquetDefaultProcessor(ParquetDefaultProcessorConfig {
            backfill_table: HashSet::from(["move_resources".to_string()]),
            channel_size: 10,
            max_buffer_size: 100000,
            upload_interval: 1800,
        });

        let result = config.get_processor_status_table_names();
        assert!(result.is_ok());

        let table_names = result.unwrap();
        let table_names: HashSet<String> = table_names.into_iter().collect();
        let expected_names: HashSet<String> =
            ["move_resources".to_string()].iter().cloned().collect();
        assert_eq!(table_names, expected_names);
    }

    #[test]
    fn test_invalid_table_name() {
        let config = ProcessorConfig::ParquetDefaultProcessor(ParquetDefaultProcessorConfig {
            backfill_table: HashSet::from(["InvalidTable".to_string(), "transactions".to_string()]),
            channel_size: 10,
            max_buffer_size: 100000,
            upload_interval: 1800,
        });

        let result = config.get_processor_status_table_names();
        assert!(result.is_err());

        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("Invalid table name 'InvalidTable'"));
        assert!(error_message.contains("Expected one of:"));
    }

    #[test]
    fn test_empty_backfill_tables() {
        let config = ProcessorConfig::ParquetDefaultProcessor(ParquetDefaultProcessorConfig {
            backfill_table: HashSet::new(),
            channel_size: 10,
            max_buffer_size: 100000,
            upload_interval: 1800,
        });
        let result = config.get_processor_status_table_names();
        assert!(result.is_ok());

        let table_names = result.unwrap();
        let expected_names: HashSet<String> = [
            "move_resources".to_string(),
            "transactions".to_string(),
            "write_set_changes".to_string(),
            "table_items".to_string(),
            "move_modules".to_string(),
            "block_metadata_transactions".to_string(),
            "current_table_items".to_string(),
            "table_metadata".to_string(),
        ]
        .iter()
        .map(|e| format!("parquet_default_processor.{e}"))
        .collect();

        let table_names: HashSet<String> = table_names.into_iter().collect();
        assert_eq!(table_names, expected_names);
    }

    #[test]
    fn test_duplicate_table_names_in_backfill_names() {
        let config = ProcessorConfig::ParquetDefaultProcessor(ParquetDefaultProcessorConfig {
            backfill_table: HashSet::from(["transactions".to_string(), "transactions".to_string()]),
            channel_size: 10,
            max_buffer_size: 100000,
            upload_interval: 1800,
        });

        let result = config.get_processor_status_table_names();
        assert!(result.is_ok());

        let table_names = result.unwrap();
        assert_eq!(table_names, vec!["transactions".to_string(),]);
    }
}
