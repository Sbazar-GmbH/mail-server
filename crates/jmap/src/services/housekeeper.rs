/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{
    collections::BinaryHeap,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant, SystemTime},
};

use common::{
    config::telemetry::OtelMetrics,
    core::BuildServer,
    ipc::{HousekeeperEvent, PurgeType},
    Inner,
};

#[cfg(feature = "enterprise")]
use common::telemetry::{
    metrics::store::{MetricsStore, SharedMetricHistory},
    tracers::store::TracingStore,
};

use smtp::reporting::SmtpReporting;
use store::write::{now, purge::PurgeStore};
use tokio::sync::mpsc;
use trc::{Collector, MetricType};
use utils::map::ttl_dashmap::TtlMap;

use crate::{email::delete::EmailDeletion, JmapMethods, LONG_SLUMBER};

#[derive(PartialEq, Eq)]
struct Action {
    due: Instant,
    event: ActionClass,
}

#[derive(PartialEq, Eq, Debug)]
enum ActionClass {
    Session,
    Account,
    Store(usize),
    Acme(String),
    OtelMetrics,
    #[cfg(feature = "enterprise")]
    InternalMetrics,
    CalculateMetrics,
    #[cfg(feature = "enterprise")]
    AlertMetrics,
    #[cfg(feature = "enterprise")]
    ValidateLicense,
}

#[derive(Default)]
struct Queue {
    heap: BinaryHeap<Action>,
}

#[cfg(feature = "enterprise")]
const METRIC_ALERTS_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub fn spawn_housekeeper(inner: Arc<Inner>, mut rx: mpsc::Receiver<HousekeeperEvent>) {
    tokio::spawn(async move {
        trc::event!(Housekeeper(trc::HousekeeperEvent::Start));
        let start_time = SystemTime::now();

        // Add all events to queue
        let mut queue = Queue::default();
        {
            let server = inner.build_server();

            // Session purge
            queue.schedule(
                Instant::now() + server.core.jmap.session_purge_frequency.time_to_next(),
                ActionClass::Session,
            );

            // Account purge
            queue.schedule(
                Instant::now() + server.core.jmap.account_purge_frequency.time_to_next(),
                ActionClass::Account,
            );

            // Store purges
            for (idx, schedule) in server.core.storage.purge_schedules.iter().enumerate() {
                queue.schedule(
                    Instant::now() + schedule.cron.time_to_next(),
                    ActionClass::Store(idx),
                );
            }

            // OTEL Push Metrics
            if let Some(otel) = &server.core.metrics.otel {
                OtelMetrics::enable_errors();
                queue.schedule(Instant::now() + otel.interval, ActionClass::OtelMetrics);
            }

            // Calculate expensive metrics
            queue.schedule(Instant::now(), ActionClass::CalculateMetrics);

            // Add all ACME renewals to heap
            for provider in server.core.acme.providers.values() {
                match server.init_acme(provider).await {
                    Ok(renew_at) => {
                        queue.schedule(
                            Instant::now() + renew_at,
                            ActionClass::Acme(provider.id.clone()),
                        );
                    }
                    Err(err) => {
                        trc::error!(err.details("Failed to initialize ACME certificate manager."));
                    }
                };
            }

            // SPDX-SnippetBegin
            // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
            // SPDX-License-Identifier: LicenseRef-SEL

            // Enterprise Edition license management
            #[cfg(feature = "enterprise")]
            if let Some(enterprise) = &server.core.enterprise {
                queue.schedule(
                    Instant::now() + enterprise.license.expires_in(),
                    ActionClass::ValidateLicense,
                );

                if let Some(metrics_store) = enterprise.metrics_store.as_ref() {
                    queue.schedule(
                        Instant::now() + metrics_store.interval.time_to_next(),
                        ActionClass::InternalMetrics,
                    );
                }

                if !enterprise.metrics_alerts.is_empty() {
                    queue.schedule(
                        Instant::now() + METRIC_ALERTS_INTERVAL,
                        ActionClass::AlertMetrics,
                    );
                }
            }
            // SPDX-SnippetEnd
        }

        // Metrics history
        #[cfg(feature = "enterprise")]
        let metrics_history = SharedMetricHistory::default();
        let mut next_metric_update = Instant::now();

        loop {
            match tokio::time::timeout(queue.wake_up_time(), rx.recv()).await {
                Ok(Some(event)) => match event {
                    HousekeeperEvent::ReloadSettings => {
                        let server = inner.build_server();

                        // Reload OTEL push metrics
                        match &server.core.metrics.otel {
                            Some(otel) if !queue.has_action(&ActionClass::OtelMetrics) => {
                                OtelMetrics::enable_errors();

                                queue.schedule(
                                    Instant::now() + otel.interval,
                                    ActionClass::OtelMetrics,
                                );
                            }
                            _ => {}
                        }

                        // SPDX-SnippetBegin
                        // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                        // SPDX-License-Identifier: LicenseRef-SEL
                        #[cfg(feature = "enterprise")]
                        if let Some(enterprise) = &server.core.enterprise {
                            if !queue.has_action(&ActionClass::ValidateLicense) {
                                queue.schedule(
                                    Instant::now() + enterprise.license.expires_in(),
                                    ActionClass::ValidateLicense,
                                );
                            }

                            if let Some(metrics_store) = enterprise.metrics_store.as_ref() {
                                if !queue.has_action(&ActionClass::InternalMetrics) {
                                    queue.schedule(
                                        Instant::now() + metrics_store.interval.time_to_next(),
                                        ActionClass::InternalMetrics,
                                    );
                                }
                            }

                            if !enterprise.metrics_alerts.is_empty()
                                && !queue.has_action(&ActionClass::AlertMetrics)
                            {
                                queue.schedule(Instant::now(), ActionClass::AlertMetrics);
                            }
                        }
                        // SPDX-SnippetEnd

                        // Reload ACME certificates
                        tokio::spawn(async move {
                            for provider in server.core.acme.providers.values() {
                                match server.init_acme(provider).await {
                                    Ok(renew_at) => {
                                        server
                                            .inner
                                            .ipc
                                            .housekeeper_tx
                                            .send(HousekeeperEvent::AcmeReschedule {
                                                provider_id: provider.id.clone(),
                                                renew_at: Instant::now() + renew_at,
                                            })
                                            .await
                                            .ok();
                                    }
                                    Err(err) => {
                                        trc::error!(err
                                            .details("Failed to reload ACME certificate manager."));
                                    }
                                };
                            }
                        });
                    }
                    HousekeeperEvent::AcmeReschedule {
                        provider_id,
                        renew_at,
                    } => {
                        let action = ActionClass::Acme(provider_id);
                        queue.remove_action(&action);
                        queue.schedule(renew_at, action);
                    }
                    HousekeeperEvent::Purge(purge) => match purge {
                        PurgeType::Data(store) => {
                            // SPDX-SnippetBegin
                            // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                            // SPDX-License-Identifier: LicenseRef-SEL
                            #[cfg(feature = "enterprise")]
                            let trace_retention = inner
                                .shared_core
                                .load()
                                .enterprise
                                .as_ref()
                                .and_then(|e| e.trace_store.as_ref())
                                .and_then(|t| t.retention);
                            #[cfg(feature = "enterprise")]
                            let metrics_retention = inner
                                .shared_core
                                .load()
                                .enterprise
                                .as_ref()
                                .and_then(|e| e.metrics_store.as_ref())
                                .and_then(|m| m.retention);
                            // SPDX-SnippetEnd

                            tokio::spawn(async move {
                                trc::event!(
                                    Housekeeper(trc::HousekeeperEvent::PurgeStore),
                                    Type = "data"
                                );
                                if let Err(err) = store.purge_store().await {
                                    trc::error!(err.details("Failed to purge data store"));
                                }

                                // SPDX-SnippetBegin
                                // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                                // SPDX-License-Identifier: LicenseRef-SEL
                                #[cfg(feature = "enterprise")]
                                if let Some(trace_retention) = trace_retention {
                                    if let Err(err) = store.purge_spans(trace_retention).await {
                                        trc::error!(err.details("Failed to purge tracing spans"));
                                    }
                                }

                                #[cfg(feature = "enterprise")]
                                if let Some(metrics_retention) = metrics_retention {
                                    if let Err(err) = store.purge_metrics(metrics_retention).await {
                                        trc::error!(err.details("Failed to purge metrics"));
                                    }
                                }
                                // SPDX-SnippetEnd
                            });
                        }
                        PurgeType::Blobs { store, blob_store } => {
                            trc::event!(
                                Housekeeper(trc::HousekeeperEvent::PurgeStore),
                                Type = "blob"
                            );

                            tokio::spawn(async move {
                                if let Err(err) = store.purge_blobs(blob_store).await {
                                    trc::error!(err.details("Failed to purge blob store"));
                                }
                            });
                        }
                        PurgeType::Lookup(store) => {
                            trc::event!(
                                Housekeeper(trc::HousekeeperEvent::PurgeStore),
                                Type = "lookup"
                            );

                            tokio::spawn(async move {
                                if let Err(err) = store.purge_lookup_store().await {
                                    trc::error!(err.details("Failed to purge lookup store"));
                                }
                            });
                        }
                        PurgeType::Account(account_id) => {
                            let server = inner.build_server();
                            tokio::spawn(async move {
                                trc::event!(Housekeeper(trc::HousekeeperEvent::PurgeAccounts));

                                if let Some(account_id) = account_id {
                                    server.purge_account(account_id).await;
                                } else {
                                    server.purge_accounts().await;
                                }
                            });
                        }
                    },
                    HousekeeperEvent::Exit => {
                        trc::event!(Housekeeper(trc::HousekeeperEvent::Stop));

                        return;
                    }
                },
                Ok(None) => {
                    trc::event!(Housekeeper(trc::HousekeeperEvent::Stop));
                    return;
                }
                Err(_) => {
                    let server = inner.build_server();
                    while let Some(event) = queue.pop() {
                        match event.event {
                            ActionClass::Acme(provider_id) => {
                                let server = server.clone();
                                tokio::spawn(async move {
                                    if let Some(provider) =
                                        server.core.acme.providers.get(&provider_id)
                                    {
                                        trc::event!(
                                            Acme(trc::AcmeEvent::OrderStart),
                                            Hostname = provider.domains.as_slice()
                                        );

                                        let renew_at = match server.renew(provider).await {
                                            Ok(renew_at) => {
                                                trc::event!(
                                                    Acme(trc::AcmeEvent::OrderCompleted),
                                                    Domain = provider.domains.as_slice(),
                                                    Expires = trc::Value::Timestamp(
                                                        now() + renew_at.as_secs()
                                                    )
                                                );

                                                renew_at
                                            }
                                            Err(err) => {
                                                trc::error!(
                                                    err.details("Failed to renew certificates.")
                                                );

                                                Duration::from_secs(3600)
                                            }
                                        };

                                        server.increment_config_version();

                                        server
                                            .inner
                                            .ipc
                                            .housekeeper_tx
                                            .send(HousekeeperEvent::AcmeReschedule {
                                                provider_id: provider_id.clone(),
                                                renew_at: Instant::now() + renew_at,
                                            })
                                            .await
                                            .ok();
                                    }
                                });
                            }
                            ActionClass::Account => {
                                let server = server.clone();
                                queue.schedule(
                                    Instant::now()
                                        + server.core.jmap.account_purge_frequency.time_to_next(),
                                    ActionClass::Account,
                                );
                                tokio::spawn(async move {
                                    trc::event!(Housekeeper(trc::HousekeeperEvent::PurgeAccounts));
                                    server.purge_accounts().await;
                                });
                            }
                            ActionClass::Session => {
                                let server = server.clone();
                                queue.schedule(
                                    Instant::now()
                                        + server.core.jmap.session_purge_frequency.time_to_next(),
                                    ActionClass::Session,
                                );

                                tokio::spawn(async move {
                                    trc::event!(Housekeeper(trc::HousekeeperEvent::PurgeSessions));
                                    server.inner.data.http_auth_cache.cleanup();
                                    server
                                        .inner
                                        .data
                                        .jmap_limiter
                                        .retain(|_, limiter| limiter.is_active());
                                    server.inner.data.access_tokens.cleanup();

                                    for throttle in [
                                        &server.inner.data.smtp_session_throttle,
                                        &server.inner.data.smtp_queue_throttle,
                                    ] {
                                        throttle.retain(|_, v| {
                                            v.concurrent.load(Ordering::Relaxed) > 0
                                        });
                                    }
                                });
                            }
                            ActionClass::Store(idx) => {
                                if let Some(schedule) =
                                    server.core.storage.purge_schedules.get(idx).cloned()
                                {
                                    queue.schedule(
                                        Instant::now() + schedule.cron.time_to_next(),
                                        ActionClass::Store(idx),
                                    );
                                    tokio::spawn(async move {
                                        let (class, result) = match schedule.store {
                                            PurgeStore::Data(store) => {
                                                ("data", store.purge_store().await)
                                            }
                                            PurgeStore::Blobs { store, blob_store } => {
                                                ("blob", store.purge_blobs(blob_store).await)
                                            }
                                            PurgeStore::Lookup(lookup_store) => {
                                                ("lookup", lookup_store.purge_lookup_store().await)
                                            }
                                        };

                                        match result {
                                            Ok(_) => {
                                                trc::event!(
                                                    Housekeeper(trc::HousekeeperEvent::PurgeStore),
                                                    Id = schedule.store_id
                                                );
                                            }
                                            Err(err) => {
                                                trc::error!(err
                                                    .details(format!(
                                                        "Failed to purge {class} store."
                                                    ))
                                                    .id(schedule.store_id));
                                            }
                                        }
                                    });
                                }
                            }
                            ActionClass::OtelMetrics => {
                                if let Some(otel) = &server.core.metrics.otel {
                                    queue.schedule(
                                        Instant::now() + otel.interval,
                                        ActionClass::OtelMetrics,
                                    );

                                    let otel = otel.clone();

                                    #[cfg(feature = "enterprise")]
                                    let is_enterprise = server.is_enterprise_edition();

                                    #[cfg(not(feature = "enterprise"))]
                                    let is_enterprise = false;

                                    tokio::spawn(async move {
                                        otel.push_metrics(is_enterprise, start_time).await;
                                    });
                                }
                            }
                            ActionClass::CalculateMetrics => {
                                // Calculate expensive metrics every 5 minutes
                                queue.schedule(
                                    Instant::now() + Duration::from_secs(5 * 60),
                                    ActionClass::OtelMetrics,
                                );

                                let update_other_metrics = if Instant::now() >= next_metric_update {
                                    next_metric_update =
                                        Instant::now() + Duration::from_secs(86400);
                                    true
                                } else {
                                    false
                                };

                                let server = server.clone();
                                tokio::spawn(async move {
                                    #[cfg(feature = "enterprise")]
                                    if server.is_enterprise_edition() {
                                        // Obtain queue size
                                        match server.total_queued_messages().await {
                                            Ok(total) => {
                                                Collector::update_gauge(
                                                    MetricType::QueueCount,
                                                    total,
                                                );
                                            }
                                            Err(err) => {
                                                trc::error!(
                                                    err.details("Failed to obtain queue size")
                                                );
                                            }
                                        }
                                    }

                                    if update_other_metrics {
                                        match server.total_accounts().await {
                                            Ok(total) => {
                                                Collector::update_gauge(
                                                    MetricType::UserCount,
                                                    total,
                                                );
                                            }
                                            Err(err) => {
                                                trc::error!(
                                                    err.details("Failed to obtain account count")
                                                );
                                            }
                                        }

                                        match server.total_domains().await {
                                            Ok(total) => {
                                                Collector::update_gauge(
                                                    MetricType::DomainCount,
                                                    total,
                                                );
                                            }
                                            Err(err) => {
                                                trc::error!(
                                                    err.details("Failed to obtain domain count")
                                                );
                                            }
                                        }
                                    }

                                    match tokio::task::spawn_blocking(memory_stats::memory_stats)
                                        .await
                                    {
                                        Ok(Some(stats)) => {
                                            Collector::update_gauge(
                                                MetricType::ServerMemory,
                                                stats.physical_mem as u64,
                                            );
                                        }
                                        Ok(None) => {}
                                        Err(err) => {
                                            trc::error!(trc::EventType::Server(
                                                trc::ServerEvent::ThreadError,
                                            )
                                            .reason(err)
                                            .caused_by(trc::location!())
                                            .details("Join Error"));
                                        }
                                    }
                                });
                            }

                            // SPDX-SnippetBegin
                            // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                            // SPDX-License-Identifier: LicenseRef-SEL
                            #[cfg(feature = "enterprise")]
                            ActionClass::InternalMetrics => {
                                if let Some(metrics_store) = &server
                                    .core
                                    .enterprise
                                    .as_ref()
                                    .and_then(|e| e.metrics_store.as_ref())
                                {
                                    queue.schedule(
                                        Instant::now() + metrics_store.interval.time_to_next(),
                                        ActionClass::InternalMetrics,
                                    );

                                    let metrics_store = metrics_store.store.clone();
                                    let metrics_history = metrics_history.clone();
                                    let core = server.core.clone();
                                    tokio::spawn(async move {
                                        if let Err(err) = metrics_store
                                            .write_metrics(core, now(), metrics_history)
                                            .await
                                        {
                                            trc::error!(err.details("Failed to write metrics"));
                                        }
                                    });
                                }
                            }

                            #[cfg(feature = "enterprise")]
                            ActionClass::AlertMetrics => {
                                let server = server.clone();

                                tokio::spawn(async move {
                                    if let Some(messages) = server.process_alerts().await {
                                        for message in messages {
                                            server
                                                .send_autogenerated(
                                                    message.from,
                                                    message.to.into_iter(),
                                                    message.body,
                                                    None,
                                                    0,
                                                )
                                                .await;
                                        }
                                    }
                                });
                            }

                            #[cfg(feature = "enterprise")]
                            ActionClass::ValidateLicense => {
                                match server.reload().await {
                                    Ok(result) => {
                                        if let Some(new_core) = result.new_core {
                                            if let Some(enterprise) = &new_core.enterprise {
                                                queue.schedule(
                                                    Instant::now()
                                                        + enterprise.license.expires_in(),
                                                    ActionClass::ValidateLicense,
                                                );
                                            }

                                            // Update core
                                            server.inner.shared_core.store(new_core.into());

                                            // Increment version counter
                                            server.increment_config_version();
                                        }
                                    }
                                    Err(err) => {
                                        trc::error!(err.details("Failed to reload configuration."));
                                    }
                                }
                            } // SPDX-SnippetEnd
                        }
                    }
                }
            }
        }
    });
}

impl Queue {
    pub fn schedule(&mut self, due: Instant, event: ActionClass) {
        trc::event!(
            Housekeeper(trc::HousekeeperEvent::Schedule),
            Due = trc::Value::Timestamp(
                now() + due.saturating_duration_since(Instant::now()).as_secs()
            ),
            Id = format!("{:?}", event)
        );

        self.heap.push(Action { due, event });
    }

    pub fn remove_action(&mut self, event: &ActionClass) {
        self.heap.retain(|e| &e.event != event);
    }

    pub fn wake_up_time(&self) -> Duration {
        self.heap
            .peek()
            .map(|e| e.due.saturating_duration_since(Instant::now()))
            .unwrap_or(LONG_SLUMBER)
    }

    pub fn pop(&mut self) -> Option<Action> {
        if self.heap.peek()?.due <= Instant::now() {
            self.heap.pop()
        } else {
            None
        }
    }

    pub fn has_action(&self, event: &ActionClass) -> bool {
        self.heap.iter().any(|e| &e.event == event)
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.due.cmp(&other.due).reverse()
    }
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
