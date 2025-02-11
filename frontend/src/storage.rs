//
// Copyright 2022 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_dynamodb::{
    error::{DeleteItemError, DeleteItemErrorKind, UpdateItemError, UpdateItemErrorKind},
    model::{AttributeValue, ReturnValue, Select},
    Client, Config,
};
use aws_smithy_async::rt::sleep::default_async_sleep;
use aws_smithy_types::{retry::RetryConfigBuilder, timeout::TimeoutConfig};
use aws_types::region::Region;
use calling_common::Duration;
use hyper::client::HttpConnector;
use hyper::{Body, Method, Request};
use log::*;
use serde::{Deserialize, Serialize};
use serde_dynamo::{from_item, to_item, Item};
use serde_with::serde_as;
use tokio::{io::AsyncWriteExt, sync::oneshot::Receiver};

use std::{collections::HashMap, path::PathBuf, time::SystemTime};

#[cfg(test)]
use mockall::{automock, predicate::*};

use crate::{
    config,
    frontend::{RoomId, UserId},
    metrics::Timer,
};

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", tag = "recordType", rename = "ActiveCall")]
pub struct CallRecord {
    /// The room that the client is authorized to join.
    /// Provided to the frontend by the client.
    pub room_id: RoomId,
    /// A random id generated and sent back to the client to let it know
    /// about the specific call "in" the room.
    ///
    /// Also used as the call ID within the backend.
    pub era_id: String,
    /// The IP of the backend Calling Server that hosts the call.
    pub backend_ip: String,
    /// The region of the backend Calling Server that hosts the call.
    #[serde(rename = "region")]
    pub backend_region: String,
    /// The ID of the user that created the call.
    ///
    /// This will not be a plain UUID; it will be encoded in some way that clients can identify.
    pub creator: UserId,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CallLinkRestrictions {
    None,
    AdminApproval,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", tag = "recordType")]
pub struct CallLinkState {
    /// Uniquely identifies the call link / the room.
    pub room_id: RoomId,
    /// Bytes chosen by the room creator to identify admins.
    #[serde(with = "serde_bytes")]
    pub admin_passkey: Vec<u8>,
    /// A serialized CallLinkPublicParams, used to verify credentials.
    #[serde(with = "serde_bytes")]
    pub zkparams: Vec<u8>,
    /// Controls access to the room.
    pub restrictions: CallLinkRestrictions,
    /// The name of the room, decryptable by clients who know the call link's root key.
    ///
    /// May be empty.
    #[serde(with = "serde_bytes")]
    pub encrypted_name: Vec<u8>,
    /// Whether or not the call link has been manually revoked.
    pub revoked: bool,
    /// When the link expires.
    ///
    /// Note that records are preserved after expiration, at least for a while, so clients can fetch
    /// the name of an expired link.
    #[serde_as(as = "serde_with::TimestampSeconds<i64>")]
    pub expiration: SystemTime,
}

impl CallLinkState {
    pub fn new(
        room_id: RoomId,
        admin_passkey: Vec<u8>,
        zkparams: Vec<u8>,
        now: SystemTime,
    ) -> Self {
        Self {
            room_id,
            admin_passkey,
            zkparams,
            restrictions: CallLinkRestrictions::None,
            encrypted_name: vec![],
            revoked: false,
            expiration: now + std::time::Duration::from_secs(60 * 60 * 24 * 90),
        }
    }
}

#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", tag = "recordType", rename = "CallLinkState")]
pub struct CallLinkUpdate {
    /// Bytes chosen by the room creator to identify admins.
    #[serde(with = "serde_bytes")]
    pub admin_passkey: Vec<u8>,
    /// Controls access to the room. If None, will not be updated.
    pub restrictions: Option<CallLinkRestrictions>,
    /// The name of the room, decryptable by clients who know the call link's root key.
    ///
    /// May be empty. If None, will not be updated.
    #[serde(with = "serde_bytes")]
    pub encrypted_name: Option<Vec<u8>>,
    /// Whether or not the call link has been manually revoked. If None, will not be updated.
    pub revoked: Option<bool>,
}

#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error(transparent)]
    UnexpectedError(#[from] anyhow::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum CallLinkUpdateError {
    #[error("room does not exist")]
    RoomDoesNotExist,
    #[error("admin passkey does not match")]
    AdminPasskeyDidNotMatch,
    #[error(transparent)]
    UnexpectedError(#[from] anyhow::Error),
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Storage: Sync + Send {
    /// Gets an existing call from the table matching the given room_id or returns None.
    async fn get_call_record(&self, room_id: &RoomId) -> Result<Option<CallRecord>, StorageError>;
    /// Adds the given call to the table but if there is already a call with the same
    /// room_id, returns that instead.
    async fn get_or_add_call_record(&self, call: CallRecord) -> Result<CallRecord, StorageError>;
    /// Removes the given call from the table as long as the era_id of the record that
    /// exists in the table is the same.
    async fn remove_call_record(&self, room_id: &RoomId, era_id: &str) -> Result<(), StorageError>;
    /// Returns a list of all calls in the table that are in the given region.
    async fn get_call_records_for_region(
        &self,
        region: &str,
    ) -> Result<Vec<CallRecord>, StorageError>;

    /// Fetches the current state for a call link.
    async fn get_call_link(&self, room_id: &RoomId) -> Result<Option<CallLinkState>, StorageError>;
    /// Updates some or all of a call link's attributes.
    async fn update_call_link(
        &self,
        room_id: &RoomId,
        new_attributes: CallLinkUpdate,
        zkparams_for_creation: Option<Vec<u8>>,
    ) -> Result<CallLinkState, CallLinkUpdateError>;
    /// Fetches both the current state for a call link and the call record
    async fn get_call_link_and_record(
        &self,
        room_id: &RoomId,
    ) -> Result<(Option<CallLinkState>, Option<CallRecord>), StorageError>;
}

pub struct DynamoDb {
    client: Client,
    table_name: String,
}

impl DynamoDb {
    pub async fn new(config: &'static config::Config) -> Result<Self> {
        let sleep_impl =
            default_async_sleep().ok_or_else(|| anyhow!("failed to create sleep_impl"))?;

        let client = match &config.storage_endpoint {
            Some(endpoint) => {
                const KEY: &str = "DUMMY_KEY";
                const PASSWORD: &str = "DUMMY_PASSWORD";

                info!("Using endpoint for DynamodDB testing: {}", endpoint);

                let aws_config = Config::builder()
                    .credentials_provider(Credentials::from_keys(KEY, PASSWORD, None))
                    .endpoint_url(endpoint)
                    .sleep_impl(sleep_impl)
                    .region(Region::new(&config.storage_region))
                    .build();
                Client::from_conf(aws_config)
            }
            _ => {
                info!(
                    "Using region for DynamodDB access: {}",
                    config.storage_region.as_str()
                );

                let retry_config = RetryConfigBuilder::new()
                    .max_attempts(4)
                    .initial_backoff(std::time::Duration::from_millis(100))
                    .build();

                let timeout_config = TimeoutConfig::builder()
                    .operation_timeout(core::time::Duration::from_secs(30))
                    .operation_attempt_timeout(core::time::Duration::from_secs(10))
                    .read_timeout(core::time::Duration::from_millis(3100))
                    .connect_timeout(core::time::Duration::from_millis(3100))
                    .build();

                let aws_config = aws_config::from_env()
                    .sleep_impl(sleep_impl)
                    .retry_config(retry_config)
                    .timeout_config(timeout_config)
                    .region(Region::new(&config.storage_region))
                    .load()
                    .await;

                Client::new(&aws_config)
            }
        };

        Ok(Self {
            client,
            table_name: config.storage_table.to_string(),
        })
    }
}

/// A wrapper around [`Item`] that can generate "upsert"-like update expressions.
///
/// Note that if there *is* an existing record, but it does *not* have all of the attributes
/// specified, those attributes will be added to the existing record. This differs from a
/// conditional expression, which will leave an existing record untouched.
///
/// ```dynamodb
/// SET #foo = if_not_exists(#foo, :foo), #bar = if_not_exists(#bar, :bar)
/// ```
struct UpsertableItem {
    partition_key: &'static str,
    sort_key: &'static str,
    update_attributes: Item,
    default_attributes: Item,
}

impl UpsertableItem {
    fn with_updates(partition_key: &'static str, sort_key: &'static str, attributes: Item) -> Self {
        Self::new(partition_key, sort_key, attributes, Default::default())
    }

    fn with_defaults(
        partition_key: &'static str,
        sort_key: &'static str,
        attributes: Item,
    ) -> Self {
        Self::new(partition_key, sort_key, Default::default(), attributes)
    }

    fn new(
        partition_key: &'static str,
        sort_key: &'static str,
        update_attributes: Item,
        default_attributes: Item,
    ) -> Self {
        Self {
            partition_key,
            sort_key,
            update_attributes,
            default_attributes,
        }
    }

    fn is_primary_key(&self, k: &str) -> bool {
        k == self.partition_key || k == self.sort_key
    }

    fn generate_update_expression(&self) -> String {
        let update_expressions = self
            .update_attributes
            .keys()
            .filter(|k| !self.is_primary_key(k))
            .map(|k| format!("#{k} = :{k}"));
        let default_expressions = self
            .default_attributes
            .keys()
            .filter(|k| !self.is_primary_key(k) && !self.update_attributes.contains_key(k.as_str()))
            .map(|k| format!("#{k} = if_not_exists(#{k}, :{k})"));

        // We don't technically need to sort the expressions, but it's better to be deterministic.
        // (And easier to test.)
        let mut expressions = update_expressions
            .chain(default_expressions)
            .collect::<Vec<_>>();
        assert!(
            !expressions.is_empty(),
            "no attributes besides primary keys, no need for upsert"
        );
        expressions.sort();
        format!("SET {}", expressions.join(","))
    }

    fn generate_attribute_names(&self) -> HashMap<String, String> {
        self.update_attributes
            .keys()
            .chain(self.default_attributes.keys())
            .filter(|k| !self.is_primary_key(k))
            .map(|k| (format!("#{k}"), k.to_string()))
            .collect()
    }

    fn into_attribute_values(mut self) -> HashMap<String, AttributeValue> {
        let update_attributes = std::mem::take(&mut self.update_attributes)
            .into_inner()
            .into_iter();
        let default_attributes = std::mem::take(&mut self.default_attributes)
            .into_inner()
            .into_iter();

        // Allow update-attributes to override default-attributes if both have an entry for the same
        // field.
        default_attributes
            .chain(update_attributes)
            .filter(|(k, _v)| !self.is_primary_key(k))
            .map(|(k, v)| (format!(":{k}"), v.into()))
            .collect()
    }
}

#[async_trait]
impl Storage for DynamoDb {
    async fn get_call_record(&self, room_id: &RoomId) -> Result<Option<CallRecord>, StorageError> {
        let response = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("roomId", AttributeValue::S(room_id.as_ref().to_string()))
            .key("recordType", AttributeValue::S("ActiveCall".to_string()))
            .consistent_read(true)
            .send()
            .await
            .context("failed to get_item from storage")?;

        Ok(response
            .item
            .map(|item| from_item(item).context("failed to convert item to CallRecord"))
            .transpose()?)
    }

    async fn get_or_add_call_record(&self, call: CallRecord) -> Result<CallRecord, StorageError> {
        let call_as_item = UpsertableItem::with_defaults(
            "roomId",
            "recordType",
            to_item(&call).expect("failed to convert CallRecord to item"),
        );
        let response = self
            .client
            .update_item()
            .table_name(&self.table_name)
            .update_expression(call_as_item.generate_update_expression())
            .key(
                call_as_item.partition_key,
                AttributeValue::S(call.room_id.as_ref().to_string()),
            )
            .key(
                call_as_item.sort_key,
                AttributeValue::S("ActiveCall".to_string()),
            )
            .set_expression_attribute_names(Some(call_as_item.generate_attribute_names()))
            .set_expression_attribute_values(Some(call_as_item.into_attribute_values()))
            .return_values(ReturnValue::AllNew)
            .send()
            .await;

        match response {
            Ok(response) => Ok(from_item(
                response.attributes().expect("requested attributes").clone(),
            )
            .context("failed to convert item to CallRecord")?),
            Err(err) => Err(StorageError::UnexpectedError(
                anyhow::Error::from(err)
                    .context("failed to update_item in storage for get_or_add_call_record"),
            )),
        }
    }

    async fn remove_call_record(&self, room_id: &RoomId, era_id: &str) -> Result<(), StorageError> {
        let response = self
            .client
            .delete_item()
            .table_name(&self.table_name)
            // Delete the item for the given key.
            .key("roomId", AttributeValue::S(room_id.as_ref().to_string()))
            .key("recordType", AttributeValue::S("ActiveCall".to_string()))
            // But only if the given era_id matches the expected value, otherwise the
            // previous call was removed and a new one created already.
            .condition_expression("eraId = :value")
            .expression_attribute_values(":value", AttributeValue::S(era_id.to_string()))
            .send()
            .await;

        match response {
            Ok(_) => Ok(()),
            Err(err) => match err.into_service_error() {
                DeleteItemError {
                    kind: DeleteItemErrorKind::ConditionalCheckFailedException(_),
                    ..
                } => Ok(()),
                err => Err(StorageError::UnexpectedError(err.into())),
            },
        }
    }

    async fn get_call_records_for_region(
        &self,
        region: &str,
    ) -> Result<Vec<CallRecord>, StorageError> {
        let response = self
            .client
            .query()
            .table_name(&self.table_name)
            .index_name("region-index")
            .key_condition_expression("#region = :value and recordType = :recordType")
            .expression_attribute_names("#region", "region")
            .expression_attribute_values(":value", AttributeValue::S(region.to_string()))
            .expression_attribute_values(":recordType", AttributeValue::S("ActiveCall".to_string()))
            .consistent_read(false)
            .select(Select::AllAttributes)
            .send()
            .await
            .context("failed to query for calls in a region")?;

        if let Some(items) = response.items {
            return Ok(items
                .into_iter()
                .map(|item| from_item(item).context("failed to convert item to CallRecord"))
                .collect::<Result<_>>()?);
        }

        Ok(vec![])
    }

    async fn get_call_link(&self, room_id: &RoomId) -> Result<Option<CallLinkState>, StorageError> {
        let response = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("roomId", AttributeValue::S(room_id.as_ref().to_string()))
            .key("recordType", AttributeValue::S("CallLinkState".to_string()))
            .consistent_read(true)
            .send()
            .await
            .context("failed to get_item from storage")?;

        Ok(response
            .item
            .map(|item| from_item(item).context("failed to convert item to CallLinkState"))
            .transpose()?)
    }

    /// Updates some or all of a call link's attributes.
    async fn update_call_link(
        &self,
        room_id: &RoomId,
        new_attributes: CallLinkUpdate,
        zkparams_for_creation: Option<Vec<u8>>,
    ) -> Result<CallLinkState, CallLinkUpdateError> {
        let mut call_as_item = UpsertableItem::with_updates(
            "roomId",
            "recordType",
            to_item(&new_attributes).expect("failed to convert CallLinkUpdate to item"),
        );

        let must_exist;
        let condition;
        if let Some(zkparams_for_creation) = zkparams_for_creation {
            call_as_item.default_attributes = to_item(CallLinkState::new(
                room_id.clone(),
                new_attributes.admin_passkey,
                zkparams_for_creation,
                SystemTime::now(),
            ))
            .expect("failed to convert CallLinkState to item");
            must_exist = false;
            condition = concat!(
                "(adminPasskey = :adminPasskey OR attribute_not_exists(adminPasskey)) AND ",
                "(zkparams = :zkparams OR attribute_not_exists(zkparams))"
            );
        } else {
            must_exist = true;
            condition = "adminPasskey = :adminPasskey";
        }

        let response = self
            .client
            .update_item()
            .table_name(&self.table_name)
            .key("roomId", AttributeValue::S(room_id.as_ref().to_string()))
            .key("recordType", AttributeValue::S("CallLinkState".to_string()))
            .update_expression(call_as_item.generate_update_expression())
            .condition_expression(condition)
            .set_expression_attribute_names(Some(call_as_item.generate_attribute_names()))
            .set_expression_attribute_values(Some(call_as_item.into_attribute_values()))
            .return_values(ReturnValue::AllNew)
            .send()
            .await;

        match response {
            Ok(response) => Ok(from_item(
                response.attributes().expect("requested attributes").clone(),
            )
            .context("failed to convert item to CallLinkState")?),
            Err(err) => match err.into_service_error() {
                UpdateItemError {
                    kind: UpdateItemErrorKind::ConditionalCheckFailedException(_),
                    ..
                } => {
                    if !must_exist {
                        // The only way this could have failed is if there *was* a room but the admin passkey (or zkparams) was wrong.
                        Err(CallLinkUpdateError::AdminPasskeyDidNotMatch)
                    } else {
                        // Check if the room exists.
                        match self.get_call_link(room_id).await {
                            Ok(Some(_)) => Err(CallLinkUpdateError::AdminPasskeyDidNotMatch),
                            Ok(None) => Err(CallLinkUpdateError::RoomDoesNotExist),
                            Err(inner_err) => Err(CallLinkUpdateError::UnexpectedError(
                                anyhow::Error::from(inner_err)
                                    .context("failed to check for existing room after failing to update_item in storage for update_call_link"),
                            ))
                        }
                    }
                }
                err => Err(CallLinkUpdateError::UnexpectedError(
                    anyhow::Error::from(err)
                        .context("failed to update_item in storage for update_call_link"),
                )),
            },
        }
    }

    async fn get_call_link_and_record(
        &self,
        room_id: &RoomId,
    ) -> Result<(Option<CallLinkState>, Option<CallRecord>), StorageError> {
        let response = self
            .client
            .query()
            .table_name(&self.table_name)
            .key_condition_expression("#roomId = :value")
            .expression_attribute_names("#roomId", "roomId")
            .expression_attribute_values(":value", AttributeValue::S(room_id.as_ref().to_string()))
            .consistent_read(true)
            .select(Select::AllAttributes)
            .send()
            .await
            .context("failed to query for call link and record from storage")?;

        let mut link_state = None;
        let mut call_record = None;

        if let Some(items) = response.items {
            for item in items {
                if let Some(AttributeValue::S(record_type)) = item.get("recordType") {
                    match record_type.as_str() {
                        "ActiveCall" => {
                            call_record = Some(
                                from_item(item).context("failed to convert item to CallRecord")?,
                            )
                        }
                        "CallLinkState" => {
                            link_state = Some(
                                from_item(item)
                                    .context("failed to convert item to CallLinkState")?,
                            )
                        }
                        &_ => {
                            warn!("unexpected record_type: {}", record_type);
                        }
                    }
                }
            }
        }

        Ok((link_state, call_record))
    }
}

/// Supports the DynamoDB storage implementation by periodically refreshing an identity
/// token file at the location given by `identity_token_path`.
pub struct IdentityFetcher {
    client: hyper::Client<HttpConnector>,
    fetch_interval: Duration,
    identity_token_path: PathBuf,
    identity_token_url: Option<String>,
}

impl IdentityFetcher {
    pub fn new(config: &'static config::Config, identity_token_path: &str) -> Self {
        IdentityFetcher {
            client: hyper::client::Client::builder().build_http(),
            fetch_interval: Duration::from_millis(config.identity_fetcher_interval_ms),
            identity_token_path: PathBuf::from(identity_token_path),
            identity_token_url: config.identity_token_url.to_owned(),
        }
    }

    pub async fn fetch_token(&self) -> Result<()> {
        if let Some(url) = &self.identity_token_url {
            let request = Request::builder()
                .method(Method::GET)
                .uri(url)
                .header("Metadata-Flavor", "Google")
                .body(Body::empty())?;

            debug!("Fetching identity token from {}", url);

            let body = self.client.request(request).await?;
            let body = hyper::body::to_bytes(body).await?;
            let temp_name = self.identity_token_path.with_extension("bak");
            let mut temp_file = tokio::fs::File::create(&temp_name).await?;
            temp_file.write_all(&body).await?;
            tokio::fs::rename(temp_name, &self.identity_token_path).await?;

            debug!(
                "Successfully wrote identity token to {:?}",
                &self.identity_token_path
            );
        }
        Ok(())
    }

    pub async fn start(self, ender_rx: Receiver<()>) -> Result<()> {
        // Periodically fetch a new web identity from GCP.
        let fetcher_handle = tokio::spawn(async move {
            loop {
                // Use sleep() instead of interval() so that we never wait *less* than one
                // interval to do the next tick.
                tokio::time::sleep(self.fetch_interval.into()).await;

                let timer = start_timer_us!("calling.frontend.identity_fetcher.timed");

                let result = &self.fetch_token().await;
                if let Err(e) = result {
                    event!("calling.frontend.identity_fetcher.error");
                    error!("Failed to fetch identity token : {:?}", e);
                }
                timer.stop();
            }
        });

        info!("fetcher ready");

        // Wait for any task to complete and cancel the rest.
        tokio::select!(
            _ = fetcher_handle => {},
            _ = ender_rx => {},
        );

        info!("fetcher shutdown");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(kv_pairs: &[(&'static str, &'static str)]) -> Item {
        kv_pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    serde_dynamo::AttributeValue::S(v.to_string()),
                )
            })
            .collect::<HashMap<_, _>>()
            .into()
    }

    #[test]
    fn upsertable_item_attribute_merging() {
        let default_attributes = make_item(&[
            ("partitionKey", "p"),
            ("sortKey", "s"),
            ("defaultOnly", "default"),
            ("defaultAndUpdate", "default"),
        ]);
        let update_attributes = make_item(&[
            ("partitionKey", "p"),
            ("sortKey", "s"),
            ("updateOnly", "update"),
            ("defaultAndUpdate", "update"),
        ]);

        let item = UpsertableItem::new(
            "partitionKey",
            "sortKey",
            update_attributes,
            default_attributes,
        );
        assert_eq!(
            item.generate_update_expression(),
            "SET #defaultAndUpdate = :defaultAndUpdate,#defaultOnly = if_not_exists(#defaultOnly, :defaultOnly),#updateOnly = :updateOnly"
        );
        assert_eq!(
            item.generate_attribute_names(),
            HashMap::from_iter(
                [
                    ("#defaultOnly", "defaultOnly"),
                    ("#defaultAndUpdate", "defaultAndUpdate"),
                    ("#updateOnly", "updateOnly")
                ]
                .map(|(k, v)| (k.to_string(), v.to_string()))
            )
        );

        assert_eq!(
            item.into_attribute_values(),
            make_item(&[
                (":defaultOnly", "default"),
                (":defaultAndUpdate", "update"),
                (":updateOnly", "update"),
            ])
            .into_inner()
            .into_iter()
            .map(|(k, v)| (k, v.into()))
            .collect()
        );
    }
}
