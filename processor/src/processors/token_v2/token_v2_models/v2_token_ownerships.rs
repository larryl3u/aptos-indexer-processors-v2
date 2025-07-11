// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

// This is required because a diesel macro makes clippy sad
#![allow(clippy::extra_unused_lifetimes)]
#![allow(clippy::unused_unit)]

use crate::{
    db::resources::FromWriteResource,
    parquet_processors::parquet_utils::util::{HasVersion, NamedTable},
    processors::{
        objects::v2_object_utils::{ObjectAggregatedDataMapping, ObjectWithMetadata},
        token_v2::{
            token_models::{
                token_utils::{TokenWriteSet, V1_TOKEN_STORE_TABLE_TYPE},
                tokens::{TableHandleToOwner, TokenV1AggregatedEventsMapping},
            },
            token_v2_models::{
                v2_token_datas::TokenDataV2,
                v2_token_utils::{
                    TokenStandard, TokenV2Burned, DEFAULT_NONE, DEFAULT_OWNER_ADDRESS,
                },
            },
        },
    },
    schema::current_token_ownerships_v2,
};
use ahash::AHashMap;
use allocative_derive::Allocative;
use anyhow::Context;
use aptos_indexer_processor_sdk::{
    aptos_protos::transaction::v1::{
        DeleteResource, DeleteTableItem, WriteResource, WriteTableItem,
    },
    postgres::utils::database::{DbContext, DbPoolConnection},
    utils::convert::{ensure_not_negative, standardize_address},
};
use bigdecimal::{BigDecimal, One, ToPrimitive, Zero};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use field_count::FieldCount;
use parquet_derive::ParquetRecordWriter;
use serde::{Deserialize, Serialize};
use tracing::error;

// PK of current_token_ownerships_v2, i.e. token_data_id, property_version_v1, owner_address, storage_id
pub type CurrentTokenOwnershipV2PK = (String, BigDecimal, String, String);

#[derive(Clone, Debug, Deserialize, FieldCount, Serialize)]
pub struct TokenOwnershipV2 {
    pub transaction_version: i64,
    pub write_set_change_index: i64,
    pub token_data_id: String,
    pub property_version_v1: BigDecimal,
    pub owner_address: Option<String>,
    pub storage_id: String,
    pub amount: BigDecimal,
    pub table_type_v1: Option<String>,
    pub token_properties_mutated_v1: Option<serde_json::Value>,
    pub is_soulbound_v2: Option<bool>,
    pub token_standard: String,
    pub is_fungible_v2: Option<bool>,
    pub transaction_timestamp: chrono::NaiveDateTime,
    pub non_transferrable_by_owner: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CurrentTokenOwnershipV2 {
    pub token_data_id: String,
    pub property_version_v1: BigDecimal,
    pub owner_address: String,
    pub storage_id: String,
    pub amount: BigDecimal,
    pub table_type_v1: Option<String>,
    pub token_properties_mutated_v1: Option<serde_json::Value>,
    pub is_soulbound_v2: Option<bool>,
    pub token_standard: String,
    pub is_fungible_v2: Option<bool>,
    pub last_transaction_version: i64,
    pub last_transaction_timestamp: chrono::NaiveDateTime,
    pub non_transferrable_by_owner: Option<bool>,
}

impl Ord for CurrentTokenOwnershipV2 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.token_data_id
            .cmp(&other.token_data_id)
            .then(self.property_version_v1.cmp(&other.property_version_v1))
            .then(self.owner_address.cmp(&other.owner_address))
            .then(self.storage_id.cmp(&other.storage_id))
    }
}

impl PartialOrd for CurrentTokenOwnershipV2 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// Facilitate tracking when a token is burned
#[derive(Clone, Debug)]
pub struct NFTOwnershipV2 {
    pub token_data_id: String,
    pub owner_address: String,
    pub is_soulbound: Option<bool>,
}

/// Need a separate struct for queryable because we don't want to define the inserted_at column (letting DB fill)
#[derive(Clone, Debug, Queryable)]
pub struct CurrentTokenOwnershipV2Query {
    pub token_data_id: String,
    pub property_version_v1: BigDecimal,
    pub owner_address: String,
    pub storage_id: String,
    pub amount: BigDecimal,
    pub table_type_v1: Option<String>,
    pub token_properties_mutated_v1: Option<serde_json::Value>,
    pub is_soulbound_v2: Option<bool>,
    pub token_standard: String,
    pub is_fungible_v2: Option<bool>,
    pub last_transaction_version: i64,
    pub last_transaction_timestamp: chrono::NaiveDateTime,
    pub inserted_at: chrono::NaiveDateTime,
    pub non_transferrable_by_owner: Option<bool>,
}

impl TokenOwnershipV2 {
    /// For nfts it's the same resources that we parse tokendatas from so we leverage the work done in there to get ownership data
    /// Vecs are returned because there could be multiple transfers in a single transaction and we need to document each one here.
    pub fn get_nft_v2_from_token_data(
        token_data: &TokenDataV2,
        object_metadatas: &ObjectAggregatedDataMapping,
    ) -> anyhow::Result<(
        Vec<Self>,
        AHashMap<CurrentTokenOwnershipV2PK, CurrentTokenOwnershipV2>,
    )> {
        let mut ownerships = vec![];
        let mut current_ownerships = AHashMap::new();

        let object_data = object_metadatas
            .get(&token_data.token_data_id)
            .context("If token data exists objectcore must exist")?;
        let object_core = object_data.object.object_core.clone();
        let token_data_id = token_data.token_data_id.clone();
        let owner_address = object_core.get_owner_address();
        let storage_id = token_data_id.clone();

        // is_soulbound currently means if an object is completely untransferrable
        // OR if only admin can transfer. Only the former is true soulbound but
        // people might already be using it with the latter meaning so let's include both.
        let is_soulbound = if object_data.untransferable.as_ref().is_some() {
            true
        } else {
            !object_core.allow_ungated_transfer
        };
        let non_transferrable_by_owner = !object_core.allow_ungated_transfer;

        ownerships.push(Self {
            transaction_version: token_data.transaction_version,
            write_set_change_index: token_data.write_set_change_index,
            token_data_id: token_data_id.clone(),
            property_version_v1: BigDecimal::zero(),
            owner_address: Some(owner_address.clone()),
            storage_id: storage_id.clone(),
            amount: BigDecimal::one(),
            table_type_v1: None,
            token_properties_mutated_v1: None,
            is_soulbound_v2: Some(is_soulbound),
            token_standard: TokenStandard::V2.to_string(),
            is_fungible_v2: None,
            transaction_timestamp: token_data.transaction_timestamp,
            non_transferrable_by_owner: Some(non_transferrable_by_owner),
        });
        current_ownerships.insert(
            (
                token_data_id.clone(),
                BigDecimal::zero(),
                owner_address.clone(),
                storage_id.clone(),
            ),
            CurrentTokenOwnershipV2 {
                token_data_id: token_data_id.clone(),
                property_version_v1: BigDecimal::zero(),
                owner_address,
                storage_id: storage_id.clone(),
                amount: BigDecimal::one(),
                table_type_v1: None,
                token_properties_mutated_v1: None,
                is_soulbound_v2: Some(is_soulbound),
                token_standard: TokenStandard::V2.to_string(),
                is_fungible_v2: None,
                last_transaction_version: token_data.transaction_version,
                last_transaction_timestamp: token_data.transaction_timestamp,
                non_transferrable_by_owner: Some(non_transferrable_by_owner),
            },
        );

        // check if token was transferred
        for (event_index, transfer_event) in &object_data.transfer_events {
            // If it's a self transfer then skip
            if transfer_event.get_to_address() == transfer_event.get_from_address() {
                continue;
            }
            ownerships.push(Self {
                transaction_version: token_data.transaction_version,
                // set to negative of event index to avoid collison with write set index
                write_set_change_index: -1 * event_index,
                token_data_id: token_data_id.clone(),
                property_version_v1: BigDecimal::zero(),
                // previous owner
                owner_address: Some(transfer_event.get_from_address()),
                storage_id: storage_id.clone(),
                // soft delete
                amount: BigDecimal::zero(),
                table_type_v1: None,
                token_properties_mutated_v1: None,
                is_soulbound_v2: Some(is_soulbound),
                token_standard: TokenStandard::V2.to_string(),
                is_fungible_v2: None,
                transaction_timestamp: token_data.transaction_timestamp,
                non_transferrable_by_owner: Some(is_soulbound),
            });
            current_ownerships.insert(
                (
                    token_data_id.clone(),
                    BigDecimal::zero(),
                    transfer_event.get_from_address(),
                    storage_id.clone(),
                ),
                CurrentTokenOwnershipV2 {
                    token_data_id: token_data_id.clone(),
                    property_version_v1: BigDecimal::zero(),
                    // previous owner
                    owner_address: transfer_event.get_from_address(),
                    storage_id: storage_id.clone(),
                    // soft delete
                    amount: BigDecimal::zero(),
                    table_type_v1: None,
                    token_properties_mutated_v1: None,
                    is_soulbound_v2: Some(is_soulbound),
                    token_standard: TokenStandard::V2.to_string(),
                    is_fungible_v2: None,
                    last_transaction_version: token_data.transaction_version,
                    last_transaction_timestamp: token_data.transaction_timestamp,
                    non_transferrable_by_owner: Some(is_soulbound),
                },
            );
        }
        Ok((ownerships, current_ownerships))
    }

    /// This handles the case where token is burned but objectCore is still there
    pub async fn get_burned_nft_v2_from_write_resource(
        write_resource: &WriteResource,
        txn_version: i64,
        write_set_change_index: i64,
        txn_timestamp: chrono::NaiveDateTime,
        prior_nft_ownership: &AHashMap<String, NFTOwnershipV2>,
        tokens_burned: &TokenV2Burned,
        object_metadatas: &ObjectAggregatedDataMapping,
        db_context: &mut Option<DbContext<'_>>,
    ) -> anyhow::Result<Option<(Self, CurrentTokenOwnershipV2)>> {
        let token_data_id = standardize_address(&write_resource.address.to_string());
        if tokens_burned
            .get(&standardize_address(&token_data_id))
            .is_some()
        {
            if let Some(object) = &ObjectWithMetadata::from_write_resource(write_resource)? {
                let object_core = &object.object_core;
                let owner_address = object_core.get_owner_address();
                let storage_id = token_data_id.clone();

                // is_soulbound currently means if an object is completely untransferrable
                // OR if only admin can transfer. Only the former is true soulbound but
                // people might already be using it with the latter meaning so let's include both.
                let is_soulbound = if object_metadatas
                    .get(&token_data_id)
                    .map(|obj| obj.untransferable.as_ref())
                    .is_some()
                {
                    true
                } else {
                    !object_core.allow_ungated_transfer
                };
                let non_transferrable_by_owner = !object_core.allow_ungated_transfer;

                return Ok(Some((
                    Self {
                        transaction_version: txn_version,
                        write_set_change_index,
                        token_data_id: token_data_id.clone(),
                        property_version_v1: BigDecimal::zero(),
                        owner_address: Some(owner_address.clone()),
                        storage_id: storage_id.clone(),
                        amount: BigDecimal::zero(),
                        table_type_v1: None,
                        token_properties_mutated_v1: None,
                        is_soulbound_v2: Some(is_soulbound),
                        token_standard: TokenStandard::V2.to_string(),
                        is_fungible_v2: Some(false),
                        transaction_timestamp: txn_timestamp,
                        non_transferrable_by_owner: Some(non_transferrable_by_owner),
                    },
                    CurrentTokenOwnershipV2 {
                        token_data_id,
                        property_version_v1: BigDecimal::zero(),
                        owner_address,
                        storage_id,
                        amount: BigDecimal::zero(),
                        table_type_v1: None,
                        token_properties_mutated_v1: None,
                        is_soulbound_v2: Some(is_soulbound),
                        token_standard: TokenStandard::V2.to_string(),
                        is_fungible_v2: Some(false),
                        last_transaction_version: txn_version,
                        last_transaction_timestamp: txn_timestamp,
                        non_transferrable_by_owner: Some(non_transferrable_by_owner),
                    },
                )));
            } else {
                return Self::get_burned_nft_v2_helper(
                    &token_data_id,
                    txn_version,
                    write_set_change_index,
                    txn_timestamp,
                    prior_nft_ownership,
                    tokens_burned,
                    db_context,
                )
                .await;
            }
        }
        Ok(None)
    }

    /// This handles the case where token is burned and objectCore is deleted
    pub async fn get_burned_nft_v2_from_delete_resource(
        delete_resource: &DeleteResource,
        txn_version: i64,
        write_set_change_index: i64,
        txn_timestamp: chrono::NaiveDateTime,
        prior_nft_ownership: &AHashMap<String, NFTOwnershipV2>,
        tokens_burned: &TokenV2Burned,
        db_context: &mut Option<DbContext<'_>>,
    ) -> anyhow::Result<Option<(Self, CurrentTokenOwnershipV2)>> {
        let token_address = standardize_address(&delete_resource.address.to_string());
        Self::get_burned_nft_v2_helper(
            &token_address,
            txn_version,
            write_set_change_index,
            txn_timestamp,
            prior_nft_ownership,
            tokens_burned,
            db_context,
        )
        .await
    }

    async fn get_burned_nft_v2_helper(
        token_address: &str,
        txn_version: i64,
        write_set_change_index: i64,
        txn_timestamp: chrono::NaiveDateTime,
        prior_nft_ownership: &AHashMap<String, NFTOwnershipV2>,
        tokens_burned: &TokenV2Burned,
        db_context: &mut Option<DbContext<'_>>,
    ) -> anyhow::Result<Option<(Self, CurrentTokenOwnershipV2)>> {
        let token_address = standardize_address(token_address);
        if let Some(burn_event) = tokens_burned.get(&token_address) {
            // 1. Try to lookup token address in burn event mapping
            let previous_owner = if let Some(previous_owner) =
                burn_event.get_previous_owner_address()
            {
                previous_owner
            } else {
                // 2. If it doesn't exist in burn event mapping, then it must be an old burn event that doesn't contain previous_owner.
                // Do a lookup to get previous owner. This is necessary because previous owner is part of current token ownerships primary key.
                match prior_nft_ownership.get(&token_address) {
                    Some(inner) => inner.owner_address.clone(),
                    None => {
                        match db_context {
                            None => {
                                tracing::debug!(
                                    transaction_version = txn_version,
                                    lookup_key = &token_address,
                                    "Avoiding db lookup for Parquet."
                                );
                                DEFAULT_OWNER_ADDRESS.to_string()
                            },
                            Some(db_context) => {
                                match CurrentTokenOwnershipV2Query::get_latest_owned_nft_by_token_data_id(
                                    &mut db_context.conn,
                                    &token_address,
                                    db_context.query_retries,
                                    db_context.query_retry_delay_ms,
                                )
                                    .await
                                {
                                    Ok(nft) => nft.owner_address.clone(),
                                    Err(_) => {
                                        tracing::warn!(
                                    transaction_version = txn_version,
                                    lookup_key = &token_address,
                                    "Failed to find current_token_ownership_v2 for burned token. You probably should backfill db."
                                );
                                        DEFAULT_OWNER_ADDRESS.to_string()
                                    },
                                }
                            }
                        }
                    },
                }
            };

            let token_data_id = token_address.clone();
            let storage_id = token_data_id.clone();

            return Ok(Some((
                Self {
                    transaction_version: txn_version,
                    write_set_change_index,
                    token_data_id: token_data_id.clone(),
                    property_version_v1: BigDecimal::zero(),
                    owner_address: Some(previous_owner.clone()),
                    storage_id: storage_id.clone(),
                    amount: BigDecimal::zero(),
                    table_type_v1: None,
                    token_properties_mutated_v1: None,
                    is_soulbound_v2: None, // default
                    token_standard: TokenStandard::V2.to_string(),
                    is_fungible_v2: None, // default
                    transaction_timestamp: txn_timestamp,
                    non_transferrable_by_owner: None, // default
                },
                CurrentTokenOwnershipV2 {
                    token_data_id,
                    property_version_v1: BigDecimal::zero(),
                    owner_address: previous_owner,
                    storage_id,
                    amount: BigDecimal::zero(),
                    table_type_v1: None,
                    token_properties_mutated_v1: None,
                    is_soulbound_v2: None, // default
                    token_standard: TokenStandard::V2.to_string(),
                    is_fungible_v2: None, // default
                    last_transaction_version: txn_version,
                    last_transaction_timestamp: txn_timestamp,
                    non_transferrable_by_owner: None, // default
                },
            )));
        }
        Ok(None)
    }

    /// We want to track tokens in any offer/claims and tokenstore
    pub fn get_v1_from_write_table_item(
        table_item: &WriteTableItem,
        txn_version: i64,
        write_set_change_index: i64,
        txn_timestamp: chrono::NaiveDateTime,
        table_handle_to_owner: &TableHandleToOwner,
        token_v1_aggregated_events: &TokenV1AggregatedEventsMapping,
    ) -> anyhow::Result<Option<(Self, Option<CurrentTokenOwnershipV2>)>> {
        let table_item_data = table_item.data.as_ref().unwrap();

        let maybe_token = match TokenWriteSet::from_table_item_type(
            table_item_data.value_type.as_str(),
            &table_item_data.value,
            txn_version,
        )? {
            Some(TokenWriteSet::Token(inner)) => Some(inner),
            _ => None,
        };

        if let Some(token) = maybe_token {
            let table_handle = standardize_address(&table_item.handle.to_string());
            let amount = ensure_not_negative(token.amount);
            let token_id_struct = token.id;
            let token_data_id_struct = token_id_struct.token_data_id;
            let token_data_id = token_data_id_struct.to_id();

            // Try to get owner from table_handle_to_owner (uses v1 events). If that doesn't exist, try to get owner
            // from token_v1_aggregated_events (uses module v2 events).
            let owner_address = match table_handle_to_owner.get(&table_handle) {
                Some(tm) if tm.table_type == V1_TOKEN_STORE_TABLE_TYPE => {
                    Some(tm.get_owner_address())
                },
                _ => token_v1_aggregated_events
                    .get(&token_data_id)
                    .and_then(|events| events.deposit_module_events.as_slice().last())
                    .and_then(|e| e.to_address.clone()),
            };

            let owner_address = match owner_address {
                Some(addr) => addr,
                None => {
                    // For token v1 offers, the token is withdrawn from the owner's account and moved
                    // into another table item. The token appears as unowned in current_token_ownerships
                    // but it will show up in the current_token_pending_claims table.
                    tracing::warn!(
                        transaction_version = txn_version,
                        table_handle = table_handle,
                        token_data_id = token_data_id,
                        "Missing table handle metadata and deposit module event for token. \
                            table_handle_to_owner: {:?}, \
                            token_v1_aggregated_events: {:?}",
                        table_handle_to_owner,
                        token_v1_aggregated_events,
                    );
                    return Ok(None);
                },
            };

            Ok(Some((
                Self {
                    transaction_version: txn_version,
                    write_set_change_index,
                    token_data_id: token_data_id.clone(),
                    property_version_v1: token_id_struct.property_version.clone(),
                    owner_address: Some(owner_address.clone()),
                    storage_id: table_handle.clone(),
                    amount: amount.clone(),
                    table_type_v1: Some(V1_TOKEN_STORE_TABLE_TYPE.to_string()),
                    token_properties_mutated_v1: Some(token.token_properties.clone()),
                    is_soulbound_v2: None,
                    token_standard: TokenStandard::V1.to_string(),
                    is_fungible_v2: None,
                    transaction_timestamp: txn_timestamp,
                    non_transferrable_by_owner: None,
                },
                Some(CurrentTokenOwnershipV2 {
                    token_data_id,
                    property_version_v1: token_id_struct.property_version,
                    owner_address,
                    storage_id: table_handle,
                    amount,
                    table_type_v1: Some(V1_TOKEN_STORE_TABLE_TYPE.to_string()),
                    token_properties_mutated_v1: Some(token.token_properties),
                    is_soulbound_v2: None,
                    token_standard: TokenStandard::V1.to_string(),
                    is_fungible_v2: None,
                    last_transaction_version: txn_version,
                    last_transaction_timestamp: txn_timestamp,
                    non_transferrable_by_owner: None,
                }),
            )))
        } else {
            Ok(None)
        }
    }

    /// We want to track tokens in any offer/claims and tokenstore
    pub fn get_v1_from_delete_table_item(
        table_item: &DeleteTableItem,
        txn_version: i64,
        write_set_change_index: i64,
        txn_timestamp: chrono::NaiveDateTime,
        table_handle_to_owner: &TableHandleToOwner,
        token_v1_aggregated_events: &TokenV1AggregatedEventsMapping,
    ) -> anyhow::Result<Option<(Self, Option<CurrentTokenOwnershipV2>)>> {
        let table_item_data = table_item.data.as_ref().unwrap();

        let maybe_token_id = match TokenWriteSet::from_table_item_type(
            table_item_data.key_type.as_str(),
            &table_item_data.key,
            txn_version,
        )? {
            Some(TokenWriteSet::TokenId(inner)) => Some(inner),
            _ => None,
        };

        if let Some(token_id_struct) = maybe_token_id {
            let table_handle = standardize_address(&table_item.handle.to_string());
            let token_data_id_struct = token_id_struct.token_data_id;
            let token_data_id = token_data_id_struct.to_id();

            // Try to get owner from table_handle_to_owner (uses v1 events). If that doesn't exist, try to get owner
            // from token_v1_aggregated_events (uses module v2 events).
            let owner_address = match table_handle_to_owner.get(&table_handle) {
                Some(tm) if tm.table_type == V1_TOKEN_STORE_TABLE_TYPE => {
                    Some(tm.get_owner_address())
                },
                _ => token_v1_aggregated_events
                    .get(&token_data_id)
                    .and_then(|events| events.withdraw_module_events.as_slice().first())
                    .and_then(|e| e.from_address.clone()),
            };
            let owner_address = match owner_address {
                Some(addr) => addr,
                None => {
                    tracing::warn!(
                        transaction_version = txn_version,
                        table_handle = table_handle,
                        token_data_id = token_data_id,
                        "Missing table handle metadata and withdraw module event for token. \
                            table_handle_to_owner: {:?}, \
                            token_v1_aggregated_events: {:?}",
                        table_handle_to_owner,
                        token_v1_aggregated_events,
                    );
                    return Ok(None);
                },
            };

            Ok(Some((
                Self {
                    transaction_version: txn_version,
                    write_set_change_index,
                    token_data_id: token_data_id.clone(),
                    property_version_v1: token_id_struct.property_version.clone(),
                    owner_address: Some(owner_address.clone()),
                    storage_id: table_handle.clone(),
                    amount: BigDecimal::zero(),
                    table_type_v1: Some(V1_TOKEN_STORE_TABLE_TYPE.to_string()),
                    token_properties_mutated_v1: None,
                    is_soulbound_v2: None,
                    token_standard: TokenStandard::V1.to_string(),
                    is_fungible_v2: None,
                    transaction_timestamp: txn_timestamp,
                    non_transferrable_by_owner: None,
                },
                Some(CurrentTokenOwnershipV2 {
                    token_data_id,
                    property_version_v1: token_id_struct.property_version,
                    owner_address,
                    storage_id: table_handle,
                    amount: BigDecimal::zero(),
                    table_type_v1: Some(V1_TOKEN_STORE_TABLE_TYPE.to_string()),
                    token_properties_mutated_v1: None,
                    is_soulbound_v2: None,
                    token_standard: TokenStandard::V1.to_string(),
                    is_fungible_v2: None,
                    last_transaction_version: txn_version,
                    last_transaction_timestamp: txn_timestamp,
                    non_transferrable_by_owner: None,
                }),
            )))
        } else {
            Ok(None)
        }
    }
}

impl CurrentTokenOwnershipV2Query {
    pub async fn get_latest_owned_nft_by_token_data_id(
        conn: &mut DbPoolConnection<'_>,
        token_data_id: &str,
        query_retries: u32,
        query_retry_delay_ms: u64,
    ) -> anyhow::Result<NFTOwnershipV2> {
        let mut tried = 0;
        while tried < query_retries {
            tried += 1;
            match Self::get_latest_owned_nft_by_token_data_id_impl(conn, token_data_id).await {
                Ok(inner) => {
                    return Ok(NFTOwnershipV2 {
                        token_data_id: inner.token_data_id.clone(),
                        owner_address: inner.owner_address.clone(),
                        is_soulbound: inner.is_soulbound_v2,
                    });
                },
                Err(_) => {
                    if tried < query_retries {
                        tokio::time::sleep(std::time::Duration::from_millis(query_retry_delay_ms))
                            .await;
                    }
                },
            }
        }
        Err(anyhow::anyhow!(
            "Failed to get nft by token data id: {}",
            token_data_id
        ))
    }

    async fn get_latest_owned_nft_by_token_data_id_impl(
        conn: &mut DbPoolConnection<'_>,
        token_data_id: &str,
    ) -> diesel::QueryResult<Self> {
        current_token_ownerships_v2::table
            .filter(current_token_ownerships_v2::token_data_id.eq(token_data_id))
            .filter(current_token_ownerships_v2::amount.gt(BigDecimal::zero()))
            .first::<Self>(conn)
            .await
    }
}

/// This is the parquet version of CurrentTokenOwnershipV2
#[derive(
    Allocative, Clone, Debug, Default, Deserialize, FieldCount, ParquetRecordWriter, Serialize,
)]
pub struct ParquetTokenOwnershipV2 {
    pub txn_version: i64,
    pub write_set_change_index: i64,
    pub token_data_id: String,
    pub property_version_v1: u64,
    pub owner_address: Option<String>,
    pub storage_id: String,
    pub amount: String, // this is a string representation of a bigdecimal
    pub table_type_v1: Option<String>,
    pub token_properties_mutated_v1: Option<String>,
    pub is_soulbound_v2: Option<bool>,
    pub token_standard: String,
    #[allocative(skip)]
    pub block_timestamp: chrono::NaiveDateTime,
    pub non_transferrable_by_owner: Option<bool>,
}

impl NamedTable for ParquetTokenOwnershipV2 {
    const TABLE_NAME: &'static str = "token_ownerships_v2";
}

impl HasVersion for ParquetTokenOwnershipV2 {
    fn version(&self) -> i64 {
        self.txn_version
    }
}

impl From<TokenOwnershipV2> for ParquetTokenOwnershipV2 {
    fn from(raw_item: TokenOwnershipV2) -> Self {
        Self {
            txn_version: raw_item.transaction_version,
            write_set_change_index: raw_item.write_set_change_index,
            token_data_id: raw_item.token_data_id,
            property_version_v1: raw_item.property_version_v1.to_u64().unwrap(),
            owner_address: raw_item.owner_address,
            storage_id: raw_item.storage_id,
            amount: raw_item.amount.to_string(),
            table_type_v1: raw_item.table_type_v1,
            token_properties_mutated_v1: raw_item
                .token_properties_mutated_v1
                .map(|v| v.to_string()),
            is_soulbound_v2: raw_item.is_soulbound_v2,
            token_standard: raw_item.token_standard,
            block_timestamp: raw_item.transaction_timestamp,
            non_transferrable_by_owner: raw_item.non_transferrable_by_owner,
        }
    }
}

#[derive(
    Allocative, Clone, Debug, Default, Deserialize, FieldCount, ParquetRecordWriter, Serialize,
)]
pub struct ParquetCurrentTokenOwnershipV2 {
    pub token_data_id: String,
    pub property_version_v1: u64, // BigDecimal,
    pub owner_address: String,
    pub storage_id: String,
    pub amount: String, // BigDecimal,
    pub table_type_v1: Option<String>,
    pub token_properties_mutated_v1: Option<String>, // Option<serde_json::Value>,
    pub is_soulbound_v2: Option<bool>,
    pub token_standard: String,
    pub is_fungible_v2: Option<bool>,
    pub last_transaction_version: i64,
    #[allocative(skip)]
    pub last_transaction_timestamp: chrono::NaiveDateTime,
    pub non_transferrable_by_owner: Option<bool>,
}

impl NamedTable for ParquetCurrentTokenOwnershipV2 {
    const TABLE_NAME: &'static str = "current_token_ownerships_v2";
}

impl HasVersion for ParquetCurrentTokenOwnershipV2 {
    fn version(&self) -> i64 {
        self.last_transaction_version
    }
}

// Facilitate tracking when a token is burned
impl From<CurrentTokenOwnershipV2> for ParquetCurrentTokenOwnershipV2 {
    fn from(raw_item: CurrentTokenOwnershipV2) -> Self {
        Self {
            token_data_id: raw_item.token_data_id,
            property_version_v1: raw_item.property_version_v1.to_u64().unwrap(),
            owner_address: raw_item.owner_address,
            storage_id: raw_item.storage_id,
            amount: raw_item.amount.to_string(),
            table_type_v1: raw_item.table_type_v1,
            token_properties_mutated_v1: raw_item
                .token_properties_mutated_v1
                .and_then(|v| {
                    canonical_json::to_string(&v)
                        .map_err(|e| {
                            error!("Failed to convert token_properties_mutated_v1: {:?}", e);
                            e
                        })
                        .ok()
                })
                .or_else(|| Some(DEFAULT_NONE.to_string())),
            is_soulbound_v2: raw_item.is_soulbound_v2,
            token_standard: raw_item.token_standard,
            is_fungible_v2: raw_item.is_fungible_v2,
            last_transaction_version: raw_item.last_transaction_version,
            last_transaction_timestamp: raw_item.last_transaction_timestamp,
            non_transferrable_by_owner: raw_item.non_transferrable_by_owner,
        }
    }
}

/// This is the postgres version of CurrentTokenOwnershipV2
#[derive(
    Clone, Debug, Deserialize, Eq, FieldCount, Identifiable, Insertable, PartialEq, Serialize,
)]
#[diesel(primary_key(token_data_id, property_version_v1, owner_address, storage_id))]
#[diesel(table_name = current_token_ownerships_v2)]
pub struct PostgresCurrentTokenOwnershipV2 {
    pub token_data_id: String,
    pub property_version_v1: BigDecimal,
    pub owner_address: String,
    pub storage_id: String,
    pub amount: BigDecimal,
    pub table_type_v1: Option<String>,
    pub token_properties_mutated_v1: Option<serde_json::Value>,
    pub is_soulbound_v2: Option<bool>,
    pub token_standard: String,
    pub is_fungible_v2: Option<bool>,
    pub last_transaction_version: i64,
    pub last_transaction_timestamp: chrono::NaiveDateTime,
    pub non_transferrable_by_owner: Option<bool>,
}

impl Ord for PostgresCurrentTokenOwnershipV2 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.token_data_id
            .cmp(&other.token_data_id)
            .then(self.property_version_v1.cmp(&other.property_version_v1))
            .then(self.owner_address.cmp(&other.owner_address))
            .then(self.storage_id.cmp(&other.storage_id))
    }
}

impl PartialOrd for PostgresCurrentTokenOwnershipV2 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<CurrentTokenOwnershipV2> for PostgresCurrentTokenOwnershipV2 {
    fn from(raw_item: CurrentTokenOwnershipV2) -> Self {
        Self {
            token_data_id: raw_item.token_data_id,
            property_version_v1: raw_item.property_version_v1,
            owner_address: raw_item.owner_address,
            storage_id: raw_item.storage_id,
            amount: raw_item.amount,
            table_type_v1: raw_item.table_type_v1,
            token_properties_mutated_v1: raw_item.token_properties_mutated_v1,
            is_soulbound_v2: raw_item.is_soulbound_v2,
            token_standard: raw_item.token_standard,
            is_fungible_v2: raw_item.is_fungible_v2,
            last_transaction_version: raw_item.last_transaction_version,
            last_transaction_timestamp: raw_item.last_transaction_timestamp,
            non_transferrable_by_owner: raw_item.non_transferrable_by_owner,
        }
    }
}
