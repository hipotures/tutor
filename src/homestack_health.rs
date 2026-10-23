//! HomeStack heartbeat: exercise the real HTTP listener and a trivial database read.
//! Telemetry itself has no timer; this is the only periodic HomeStack task.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use rocket::fairing::AdHoc;
use serde_json::Value;

use crate::{db::DbPool, homestack_status::Status};

const DEFAULT_INTERVAL: u64 = 60;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub const PROBE_PATH: &str = "/health/heartbeat";

#[derive(Default)]
struct Progress {
    last_success: Option<Instant>,
    count: u64,
}

pub struct Heartbeat {
    started: Instant,
    interval: Duration,
    progress: Mutex<Progress>,
}

pub struct Snapshot {
    pub pass: bool,
    pub age: u64,
    pub count: u64,
}

impl Snapshot {
    pub fn status(&self) -> Status {
        if self.pass {
            Status::Pass
        } else {
            Status::Fail
        }
    }

    pub fn checks(&self) -> Value {
        let status = if self.pass {
            "pass"
        } else {
            "fail"
        };
        json!({
            "heartbeat:age": [{ "status": status, "observedValue": self.age, "observedUnit": "s" }],
            "heartbeat:count": [{ "status": status, "observedValue": self.count }],
        })
    }
}

impl Heartbeat {
    pub fn from_env(started: Instant) -> Self {
        let value = std::env::var("HOMESTACK_HEARTBEAT_SECONDS").ok();
        let seconds = heartbeat_seconds(value.as_deref());
        if value.is_some() && value.as_deref().and_then(|v| v.parse::<u64>().ok()) != Some(seconds) {
            warn!("Invalid HOMESTACK_HEARTBEAT_SECONDS; using 60 seconds (allowed: 10–3600)");
        }
        Self::new(started, Duration::from_secs(seconds))
    }

    pub(crate) fn new(started: Instant, interval: Duration) -> Self {
        Self {
            started,
            interval,
            progress: Mutex::new(Progress::default()),
        }
    }

    fn succeeded(&self) {
        let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        progress.last_success = Some(Instant::now());
        progress.count = progress.count.saturating_add(1);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> Snapshot {
        let progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        let age = now.saturating_duration_since(progress.last_success.unwrap_or(self.started));
        Snapshot {
            pass: age <= self.interval * 3,
            age: age.as_secs(),
            count: progress.count,
        }
    }
}

fn heartbeat_seconds(value: Option<&str>) -> u64 {
    value.and_then(|v| v.parse().ok()).filter(|v| (10..=3600).contains(v)).unwrap_or(DEFAULT_INTERVAL)
}

/// Read a constant through the application's ordinary database pool and workers.
/// This also lets a recovered service resume heartbeats even when health is failing.
pub async fn database_ready(pool: &DbPool) -> bool {
    use diesel::{RunQueryDsl, expression::IntoSql};
    let Ok(connection) = pool.get().await else {
        return false;
    };
    // Isolate a blocked native database call from the HTTP handler's timeout.
    // The connection keeps its pool permit until the operation actually finishes.
    crate::db::run_blocking(move || {
        tokio::runtime::Handle::current().block_on(connection.run(|connection| {
            diesel::select(1_i32.into_sql::<diesel::sql_types::Integer>()).get_result::<i32>(connection).is_ok()
        }))
    })
    .await
}

pub fn fairing() -> AdHoc {
    AdHoc::on_liftoff("HomeStack heartbeat", |rocket| {
        Box::pin(async move {
            let heartbeat = Arc::clone(rocket.state::<Arc<Heartbeat>>().expect("heartbeat state"));
            let config = rocket.config();
            let address = match config.address {
                IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                address => address,
            };
            let scheme = if config.tls_enabled() {
                "https"
            } else {
                "http"
            };
            let url = format!("{scheme}://{}{PROBE_PATH}", SocketAddr::new(address, config.port));
            let client = match reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(PROBE_TIMEOUT)
                .build()
            {
                Ok(client) => client,
                Err(_) => {
                    error!("Unable to initialize HomeStack heartbeat client");
                    return;
                }
            };
            let mut shutdown = rocket.shutdown();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(heartbeat.interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = tick.tick() => {}
                    }
                    tokio::select! {
                        _ = &mut shutdown => break,
                        response = client.get(&url).send() => {
                            if response.is_ok_and(|r| r.status() == reqwest::StatusCode::NO_CONTENT) {
                                heartbeat.succeeded();
                            }
                        }
                    }
                }
            });
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homestack_heartbeat_expires_after_three_intervals_and_recovers() {
        let started = Instant::now();
        let heartbeat = Heartbeat::new(started, Duration::from_secs(10));
        let first = heartbeat.snapshot_at(started);
        assert!(first.pass);
        assert_eq!((first.age, first.count), (0, 0));
        assert!(heartbeat.snapshot_at(started + Duration::from_secs(30)).pass);
        let stale = heartbeat.snapshot_at(started + Duration::from_millis(30001));
        assert!(!stale.pass);
        assert_eq!(stale.checks()["heartbeat:age"][0]["observedUnit"], "s");
        assert_eq!(serde_json::to_value(stale.status()).unwrap(), "fail");
        heartbeat.succeeded();
        assert!(heartbeat.snapshot().pass);
        assert_eq!(heartbeat.snapshot().count, 1);
        heartbeat.succeeded();
        assert_eq!(heartbeat.snapshot().count, 2);
    }

    #[test]
    fn homestack_heartbeat_interval_defaults_and_validates() {
        assert_eq!(heartbeat_seconds(None), 60);
        for input in ["", "false", "9", "3601", "-10"] {
            assert_eq!(heartbeat_seconds(Some(input)), 60);
        }
        assert_eq!(heartbeat_seconds(Some("10")), 10);
        assert_eq!(heartbeat_seconds(Some("3600")), 3600);
    }

    #[cfg(sqlite)]
    struct Gate {
        blocked: std::sync::atomic::AtomicBool,
        released: tokio::sync::Notify,
    }

    #[cfg(sqlite)]
    #[get("/health/heartbeat")]
    async fn probe(gate: &rocket::State<Arc<Gate>>) -> rocket::http::Status {
        if gate.blocked.load(std::sync::atomic::Ordering::SeqCst) {
            gate.released.notified().await;
        }
        rocket::http::Status::NoContent
    }

    #[cfg(sqlite)]
    #[rocket::async_test]
    async fn homestack_heartbeat_exercises_http_and_detects_a_blocked_worker() {
        use std::sync::atomic::Ordering;

        crate::install_rustls_crypto_provider();
        let gate = Arc::new(Gate {
            blocked: std::sync::atomic::AtomicBool::new(false),
            released: tokio::sync::Notify::new(),
        });
        let heartbeat = Arc::new(Heartbeat::new(Instant::now(), Duration::from_millis(100)));
        let (address_tx, address_rx) = tokio::sync::oneshot::channel();
        let rocket = rocket::custom(rocket::Config {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            ..rocket::Config::debug_default()
        })
        .manage(Arc::clone(&gate))
        .manage(Arc::clone(&heartbeat))
        .manage(DbPool::homestack_test_pool())
        .mount("/", routes![probe])
        .mount("/", crate::api::homestack_health_routes())
        .attach(fairing())
        .attach(AdHoc::on_liftoff("test address", move |rocket| {
            Box::pin(async move {
                drop(address_tx.send((rocket.config().port, rocket.shutdown())));
            })
        }));
        let server = tokio::spawn(rocket.launch());
        let (port, shutdown) = address_rx.await.unwrap();
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build().unwrap();
        let origin = format!("http://127.0.0.1:{port}");
        let wait_count = async {
            while heartbeat.snapshot().count == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), wait_count).await.unwrap();
        gate.blocked.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(450)).await;
        let old_count = heartbeat.snapshot().count;
        for path in ["/health/live", "/health/ready"] {
            assert_eq!(client.get(format!("{origin}{path}")).send().await.unwrap().status().as_u16(), 503);
        }
        gate.blocked.store(false, Ordering::SeqCst);
        gate.released.notify_waiters();
        let recover = async {
            while heartbeat.snapshot().count <= old_count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), recover).await.unwrap();
        assert_eq!(client.get(format!("{origin}/health/live")).send().await.unwrap().status().as_u16(), 200);
        shutdown.notify();
        server.await.unwrap().unwrap();
    }
}
