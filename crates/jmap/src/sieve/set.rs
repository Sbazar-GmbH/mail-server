use jmap_proto::{
    error::{
        method::MethodError,
        set::{SetError, SetErrorType},
    },
    method::set::{SetRequest, SetResponse},
    object::{
        index::{IndexAs, IndexProperty, ObjectIndexBuilder},
        sieve::SetArguments,
        Object,
    },
    request::reference::MaybeReference,
    response::references::EvalObjectReferences,
    types::{
        blob::BlobId,
        collection::Collection,
        id::Id,
        property::Property,
        value::{MaybePatchValue, SetValue, Value},
    },
};
use sieve::compiler::ErrorType;
use store::{
    query::Filter,
    rand::{distributions::Alphanumeric, thread_rng, Rng},
    write::{assert::HashedValue, log::ChangeLogBuilder, BatchBuilder, F_CLEAR, F_VALUE},
    BlobKind,
};

use crate::{auth::AclToken, JMAP};

struct SetContext<'x> {
    account_id: u32,
    acl_token: &'x AclToken,
    response: SetResponse,
}

pub static SCHEMA: &[IndexProperty] = &[
    IndexProperty::new(Property::Name)
        .index_as(IndexAs::Text {
            tokenize: true,
            index: true,
        })
        .max_size(255)
        .required(),
    IndexProperty::new(Property::IsActive).index_as(IndexAs::Integer),
];

impl JMAP {
    pub async fn sieve_script_set(
        &self,
        mut request: SetRequest<SetArguments>,
        acl_token: &AclToken,
    ) -> Result<SetResponse, MethodError> {
        let account_id = acl_token.primary_id();
        let mut sieve_ids = self
            .get_document_ids(account_id, Collection::SieveScript)
            .await?
            .unwrap_or_default();
        let mut ctx = SetContext {
            account_id,
            acl_token,
            response: self
                .prepare_set_response(&request, Collection::SieveScript)
                .await?,
        };
        let will_destroy = request.unwrap_destroy();

        // Process creates
        let mut changes = ChangeLogBuilder::new();
        for (id, object) in request.unwrap_create() {
            if sieve_ids.len() as usize <= self.config.sieve_max_scripts {
                match self.sieve_set_item(object, None, &ctx).await? {
                    Ok((builder, Some(blob))) => {
                        // Obtain document id
                        let document_id = self
                            .assign_document_id(account_id, Collection::SieveScript)
                            .await?;

                        // Store blob
                        let blob_id =
                            BlobId::linked(account_id, Collection::SieveScript, document_id);
                        self.put_blob(&blob_id.kind, &blob).await?;

                        // Write record
                        let mut batch = BatchBuilder::new();
                        batch
                            .with_account_id(account_id)
                            .with_collection(Collection::SieveScript)
                            .create_document(document_id)
                            .custom(builder);
                        sieve_ids.insert(document_id);
                        self.write_batch(batch).await?;
                        changes.log_insert(Collection::SieveScript, document_id);

                        // Add result with updated blobId
                        ctx.response.created.insert(
                            id,
                            Object::with_capacity(1)
                                .with_property(Property::Id, Value::Id(document_id.into()))
                                .with_property(
                                    Property::BlobId,
                                    blob_id.with_section_size(blob.len()),
                                ),
                        );
                    }
                    Err(err) => {
                        ctx.response.not_created.append(id, err);
                    }
                    _ => unreachable!(),
                }
            } else {
                ctx.response.not_created.append(id, SetError::new(SetErrorType::OverQuota).with_description(
                    "There are too many sieve scripts, please delete some before adding a new one.",
                ));
            }
        }

        // Process updates
        'update: for (id, object) in request.unwrap_update() {
            // Make sure id won't be destroyed
            if will_destroy.contains(&id) {
                ctx.response
                    .not_updated
                    .append(id, SetError::will_destroy());
                continue 'update;
            }

            // Obtain sieve script
            let document_id = id.document_id();
            if let Some(mut sieve) = self
                .get_property::<HashedValue<Object<Value>>>(
                    account_id,
                    Collection::SieveScript,
                    document_id,
                    Property::Value,
                )
                .await?
            {
                match self
                    .sieve_set_item(object, (document_id, sieve.take()).into(), &ctx)
                    .await?
                {
                    Ok((builder, blob)) => {
                        // Store blob
                        let blob_id = if let Some(blob) = blob {
                            let blob_id =
                                BlobId::linked(account_id, Collection::SieveScript, document_id);
                            self.put_blob(&blob_id.kind, &blob).await?;
                            Some(blob_id.with_section_size(blob.len()))
                        } else {
                            None
                        };

                        // Write record
                        let mut batch = BatchBuilder::new();
                        batch
                            .with_account_id(account_id)
                            .with_collection(Collection::SieveScript)
                            .update_document(document_id)
                            .assert_value(Property::Value, &sieve)
                            .custom(builder);
                        if !batch.is_empty() {
                            changes.log_update(Collection::SieveScript, document_id);
                            match self.store.write(batch.build()).await {
                                Ok(_) => (),
                                Err(store::Error::AssertValueFailed) => {
                                    ctx.response.not_updated.append(id, SetError::forbidden().with_description(
                                        "Another process modified this sieve, please try again.",
                                    ));
                                    continue 'update;
                                }
                                Err(err) => {
                                    tracing::error!(
                                        event = "error",
                                        context = "sieve_set",
                                        account_id = account_id,
                                        error = ?err,
                                        "Failed to update sieve script(s).");
                                    return Err(MethodError::ServerPartialFail);
                                }
                            }
                        }

                        // Add result with updated blobId
                        ctx.response.updated.append(
                            id,
                            blob_id.map(|blob_id| {
                                Object::with_capacity(1).with_property(Property::BlobId, blob_id)
                            }),
                        );
                    }
                    Err(err) => {
                        ctx.response.not_updated.append(id, err);
                        continue 'update;
                    }
                }
            } else {
                ctx.response.not_updated.append(id, SetError::not_found());
            }
        }

        // Process deletions
        for id in will_destroy {
            let document_id = id.document_id();
            if sieve_ids.contains(document_id) {
                // Make sure the script is not active
                if matches!(
                    self.get_property::<Object<Value>>(
                        account_id,
                        Collection::SieveScript,
                        document_id,
                        Property::Value,
                    )
                    .await?
                    .and_then(|mut obj| obj.properties.remove(&Property::IsActive)),
                    Some(Value::Bool(true))
                ) {
                    ctx.response.not_destroyed.append(
                        id,
                        SetError::new(SetErrorType::ScriptIsActive)
                            .with_description("Deactivate Sieve script before deletion."),
                    );
                    continue;
                }
                self.sieve_script_delete(account_id, document_id).await?;
                changes.log_delete(Collection::SieveScript, document_id);
                ctx.response.destroyed.push(id);
            } else {
                ctx.response.not_destroyed.append(id, SetError::not_found());
            }
        }

        // Write changes
        if !changes.is_empty() {
            ctx.response.new_state = self.commit_changes(account_id, changes).await?.into();
        }

        // Activate / deactivate scripts
        if ctx.response.not_created.is_empty()
            && ctx.response.not_updated.is_empty()
            && ctx.response.not_destroyed.is_empty()
            && (request.arguments.on_success_activate_script.is_some()
                || request
                    .arguments
                    .on_success_deactivate_script
                    .unwrap_or(false))
        {
            let changed_ids = if let Some(id) = request.arguments.on_success_activate_script {
                self.sieve_activate_script(
                    account_id,
                    match id {
                        MaybeReference::Value(id) => id.document_id(),
                        MaybeReference::Reference(id_ref) => match ctx.response.get_id(&id_ref) {
                            Some(id) => id.document_id(),
                            None => return Ok(ctx.response),
                        },
                    }
                    .into(),
                )
                .await?
            } else {
                self.sieve_activate_script(account_id, None).await?
            };

            for (document_id, is_active) in changed_ids {
                if let Some(obj) = ctx.response.get_object_by_id(Id::from(document_id)) {
                    obj.append(Property::IsActive, Value::Bool(is_active));
                }
            }
        }

        Ok(ctx.response)
    }

    pub async fn sieve_script_delete(
        &self,
        account_id: u32,
        document_id: u32,
    ) -> Result<(), MethodError> {
        // Delete record
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(account_id)
            .with_collection(Collection::SieveScript)
            .delete_document(document_id)
            .value(Property::Value, (), F_VALUE | F_CLEAR)
            .value(Property::EmailIds, (), F_VALUE | F_CLEAR);
        self.write_batch(batch).await?;
        let _ = self
            .delete_blob(&BlobKind::Linked {
                account_id,
                collection: Collection::SieveScript.into(),
                document_id,
            })
            .await;
        Ok(())
    }

    #[allow(clippy::blocks_in_if_conditions)]
    async fn sieve_set_item(
        &self,
        changes_: Object<SetValue>,
        update: Option<(u32, Object<Value>)>,
        ctx: &SetContext<'_>,
    ) -> Result<Result<(ObjectIndexBuilder, Option<Vec<u8>>), SetError>, MethodError> {
        // Vacation script cannot be modified
        if matches!(update.as_ref().and_then(|(_, obj)| obj.properties.get(&Property::Name)), Some(Value::Text ( value )) if value.eq_ignore_ascii_case("vacation"))
        {
            return Ok(Err(SetError::forbidden().with_description(
                "The 'vacation' script cannot be modified, use VacationResponse/set instead.",
            )));
        }

        // Parse properties
        let mut changes = Object::with_capacity(changes_.properties.len());
        let mut blob_id = None;
        for (property, value) in changes_.properties {
            let value = match ctx.response.eval_object_references(value) {
                Ok(value) => value,
                Err(err) => {
                    return Ok(Err(err));
                }
            };
            let value = match (&property, value) {
                (Property::Name, MaybePatchValue::Value(Value::Text(value))) => {
                    if value.len() > self.config.sieve_max_script_name {
                        return Ok(Err(SetError::invalid_properties()
                            .with_property(property)
                            .with_description("Script name is too long.")));
                    } else if value.eq_ignore_ascii_case("vacation") {
                        return Ok(Err(SetError::forbidden()
                            .with_property(property)
                            .with_description(
                                "The 'vacation' name is reserved, please use a different name.",
                            )));
                    } else if update
                        .as_ref()
                        .and_then(|(_, obj)| obj.properties.get(&Property::Name))
                        .map_or(
                            true,
                            |p| matches!(p, Value::Text (prev_value ) if prev_value != &value),
                        )
                    {
                        if let Some(id) = self
                            .filter(
                                ctx.account_id,
                                Collection::SieveScript,
                                vec![Filter::eq(Property::Name, &value)],
                            )
                            .await?
                            .results
                            .min()
                        {
                            return Ok(Err(SetError::already_exists()
                                .with_existing_id(id.into())
                                .with_description(format!(
                                    "A sieve script with name '{}' already exists.",
                                    value
                                ))));
                        }
                    }

                    Value::Text(value)
                }
                (Property::BlobId, MaybePatchValue::Value(Value::BlobId(value))) => {
                    blob_id = value.into();
                    continue;
                }
                (Property::Name, MaybePatchValue::Value(Value::Null)) => {
                    continue;
                }
                _ => {
                    return Ok(Err(SetError::invalid_properties()
                        .with_property(property)
                        .with_description("Invalid property or value.".to_string())))
                }
            };
            changes.append(property, value);
        }

        if update.is_none() {
            // Add name if missing
            if !matches!(changes.properties.get(&Property::Name), Some(Value::Text ( value )) if !value.is_empty())
            {
                changes.set(
                    Property::Name,
                    Value::Text(
                        thread_rng()
                            .sample_iter(Alphanumeric)
                            .take(15)
                            .map(char::from)
                            .collect::<String>(),
                    ),
                );
            }

            // Set script as inactive
            changes.set(Property::IsActive, Value::Bool(false));
        }

        let blob_update = if let Some(blob_id) = blob_id {
            if update.as_ref().map_or(true, |(document_id, _)| {
                !blob_id
                    .kind
                    .is_document(ctx.account_id, Collection::SieveScript, *document_id)
            }) {
                // Check access
                if let Some(mut bytes) = self.blob_download(&blob_id, ctx.acl_token).await? {
                    // Compile script
                    match self.sieve_compiler.compile(&bytes) {
                        Ok(script) => {
                            changes.set(Property::BlobId, Value::UnsignedInt(bytes.len() as u64));
                            bytes.extend(bincode::serialize(&script).unwrap_or_default());
                            bytes.into()
                        }
                        Err(err) => {
                            return Ok(Err(SetError::new(
                                if let ErrorType::ScriptTooLong = &err.error_type() {
                                    SetErrorType::TooLarge
                                } else {
                                    SetErrorType::InvalidScript
                                },
                            )
                            .with_description(err.to_string())));
                        }
                    }
                } else {
                    return Ok(Err(SetError::new(SetErrorType::BlobNotFound)
                        .with_property(Property::BlobId)
                        .with_description("Blob does not exist.")));
                }
            } else {
                None
            }
        } else if update.is_none() {
            return Ok(Err(SetError::invalid_properties()
                .with_property(Property::BlobId)
                .with_description("Missing blobId.")));
        } else {
            None
        };

        // Validate
        Ok(ObjectIndexBuilder::new(SCHEMA)
            .with_changes(changes)
            .with_current_opt(update.map(|(_, current)| current))
            .validate()
            .map(|obj| (obj, blob_update)))
    }

    pub async fn sieve_activate_script(
        &self,
        account_id: u32,
        activate_id: Option<u32>,
    ) -> Result<Vec<(u32, bool)>, MethodError> {
        let mut changed_ids = Vec::new();
        // Find the currently active script
        let active_ids = self
            .filter(
                account_id,
                Collection::SieveScript,
                vec![Filter::eq(Property::IsActive, 1u32)],
            )
            .await?
            .results;

        // Check if script is already active
        if activate_id.map_or(false, |id| active_ids.contains(id)) {
            return Ok(changed_ids);
        }

        // Prepare batch
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(account_id)
            .with_collection(Collection::SieveScript);

        // Deactivate scripts
        for document_id in active_ids {
            if let Some(sieve) = self
                .get_property::<HashedValue<Object<Value>>>(
                    account_id,
                    Collection::SieveScript,
                    document_id,
                    Property::Value,
                )
                .await?
            {
                batch
                    .update_document(document_id)
                    .value(Property::EmailIds, (), F_VALUE | F_CLEAR)
                    .assert_value(Property::Value, &sieve)
                    .custom(
                        ObjectIndexBuilder::new(SCHEMA)
                            .with_changes(
                                Object::with_capacity(1).with_property(Property::IsActive, false),
                            )
                            .with_current(sieve.inner),
                    );
                changed_ids.push((document_id, false));
            }
        }

        // Activate script
        if let Some(document_id) = activate_id {
            if let Some(sieve) = self
                .get_property::<HashedValue<Object<Value>>>(
                    account_id,
                    Collection::SieveScript,
                    document_id,
                    Property::Value,
                )
                .await?
            {
                batch
                    .update_document(document_id)
                    .assert_value(Property::Value, &sieve)
                    .custom(
                        ObjectIndexBuilder::new(SCHEMA)
                            .with_changes(
                                Object::with_capacity(1).with_property(Property::IsActive, true),
                            )
                            .with_current(sieve.inner),
                    );
                changed_ids.push((document_id, true));
            }
        }

        // Write changes
        if !changed_ids.is_empty() {
            match self.store.write(batch.build()).await {
                Ok(_) => (),
                Err(store::Error::AssertValueFailed) => {
                    return Ok(vec![]);
                }
                Err(err) => {
                    tracing::error!(
                        event = "error",
                        context = "sieve_activate_script",
                        account_id = account_id,
                        error = ?err,
                        "Failed to activate sieve script(s).");
                    return Err(MethodError::ServerPartialFail);
                }
            }
        }

        Ok(changed_ids)
    }
}
