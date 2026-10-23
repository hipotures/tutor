use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use rocket::{
    Catcher, Route, State,
    fs::NamedFile,
    http::{ContentType, Header, Status},
    response::{Redirect, content::RawCss as Css, content::RawHtml as Html},
    serde::json::Json,
};
use serde_json::Value;

use crate::{
    CONFIG,
    api::{ApiResult, EmptyResult, core::now},
    auth::decode_file_download,
    crypto::sha256_hex,
    db::{
        DbConn, DbPool,
        models::{AttachmentId, CipherId},
    },
    error::Error,
    util::{Cached, EtagCached},
};

pub fn routes() -> Vec<Route> {
    // If adding more routes here, consider also adding them to
    // crate::utils::LOGGED_ROUTES to make sure they appear in the log
    let mut routes = routes![attachments, alive, alive_head, health_live, health_ready, static_files];
    if CONFIG.web_vault_enabled() {
        routes.append(&mut routes![
            web_index,
            web_index_direct,
            web_index_head,
            app_id,
            apple_app_site_association,
            web_files,
            vaultwarden_css
        ]);
    }

    #[cfg(debug_assertions)]
    if CONFIG.reload_templates() {
        routes.append(&mut routes![static_files_dev]);
    }

    routes
}

pub fn catchers() -> Vec<Catcher> {
    if CONFIG.web_vault_enabled() {
        catchers![not_found]
    } else {
        catchers![]
    }
}

#[catch(404)]
fn not_found() -> ApiResult<Html<String>> {
    // Return the page
    let json = json!({
        "urlpath": CONFIG.domain_path()
    });
    let text = CONFIG.render_template("404", &json)?;
    Ok(Html(text))
}

struct CssCache {
    css: String,
    etag: String,
}

static CSS_CACHE: RwLock<Option<Arc<CssCache>>> = RwLock::new(None);

pub fn invalidate_css_cache() {
    *CSS_CACHE.write().unwrap() = None;
}

#[get("/css/vaultwarden.css")]
fn vaultwarden_css() -> EtagCached<Css<String>> {
    // If reload_templates is false, and we already have the CSS Cached, return this
    if !CONFIG.reload_templates()
        && let Some(cached) = CSS_CACHE.read().unwrap().as_ref()
    {
        return EtagCached::new(Css(cached.css.clone()), &cached.etag);
    }

    // Else, there is either no cache, or reload_templates is true and we need to rebuild the CSS
    let css_options = json!({
        "email_change_allowed": CONFIG.email_change_allowed(),
        "emergency_access_allowed": CONFIG.emergency_access_allowed(),
        "load_user_scss": true,
        "mail_2fa_enabled": CONFIG._enable_email_2fa(),
        "mail_enabled": CONFIG.mail_enabled(),
        "sends_allowed": CONFIG.sends_allowed(),
        "remember_2fa_disabled": CONFIG.disable_2fa_remember(),
        "password_hints_allowed": CONFIG.password_hints_allowed(),
        "signup_disabled": CONFIG.is_signup_disabled(),
        "sso_enabled": CONFIG.sso_enabled(),
        "sso_only": CONFIG.sso_enabled() && CONFIG.sso_only(),
        "webauthn_2fa_supported": CONFIG.is_webauthn_2fa_supported(),
        "yubico_enabled": CONFIG._enable_yubico() && CONFIG.yubico_client_id().is_some() && CONFIG.yubico_secret_key().is_some(),
    });

    let scss = match CONFIG.render_template("scss/vaultwarden.scss", &css_options) {
        Ok(t) => t,
        Err(e) => {
            // Something went wrong loading the template. Use the fallback
            warn!("Loading scss/vaultwarden.scss.hbs or scss/user.vaultwarden.scss.hbs failed. {e}");
            CONFIG
                .render_fallback_template("scss/vaultwarden.scss", &css_options)
                .expect("Fallback scss/vaultwarden.scss.hbs to render")
        }
    };

    let css = match grass_compiler::from_string(
        scss,
        &grass_compiler::Options::default().style(grass_compiler::OutputStyle::Compressed),
    ) {
        Ok(css) => css,
        Err(e) => {
            // Something went wrong compiling the scss. Use the fallback
            warn!("Compiling the Vaultwarden SCSS styles failed. {e}");
            let mut css_options = css_options;
            css_options["load_user_scss"] = json!(false);
            let scss = CONFIG
                .render_fallback_template("scss/vaultwarden.scss", &css_options)
                .expect("Fallback scss/vaultwarden.scss.hbs to render");
            grass_compiler::from_string(
                scss,
                &grass_compiler::Options::default().style(grass_compiler::OutputStyle::Compressed),
            )
            .expect("SCSS to compile")
        }
    };

    let etag = sha256_hex(css.as_bytes());
    let cached = Arc::new(CssCache {
        css,
        etag,
    });

    if !CONFIG.reload_templates() {
        *CSS_CACHE.write().unwrap() = Some(Arc::clone(&cached));
    }

    // Etag Caching will let the browser send us an etag to verify and send new content if needed
    EtagCached::new(Css(cached.css.clone()), &cached.etag)
}

#[get("/")]
async fn web_index() -> Cached<Option<NamedFile>> {
    Cached::short(NamedFile::open(Path::new(&CONFIG.web_vault_folder()).join("index.html")).await.ok(), false)
}

// Make sure that `/index.html` redirect to actual domain path.
// If not, this might cause issues with the web-vault
#[get("/index.html")]
fn web_index_direct() -> Redirect {
    Redirect::to(format!("{}/", CONFIG.domain_path()))
}

#[head("/")]
fn web_index_head() -> EmptyResult {
    // Add an explicit HEAD route to prevent uptime monitoring services from
    // generating "No matching routes for HEAD /" error messages.
    //
    // Rocket automatically implements a HEAD route when there's a matching GET
    // route, but relying on this behavior also means a spurious error gets
    // logged due to <https://github.com/SergioBenitez/Rocket/issues/1098>.
    Ok(())
}

#[get("/app-id.json")]
fn app_id() -> Cached<(ContentType, Json<Value>)> {
    let content_type = ContentType::new("application", "fido.trusted-apps+json");

    Cached::long(
        (
            content_type,
            Json(json!({
            "trustedFacets": [
                {
                "version": { "major": 1, "minor": 0 },
                "ids": [
                    // Per <https://fidoalliance.org/specs/fido-v2.0-id-20180227/fido-appid-and-facets-v2.0-id-20180227.html#determining-the-facetid-of-a-calling-application>:
                    //
                    // "In the Web case, the FacetID MUST be the Web Origin [RFC6454]
                    // of the web page triggering the FIDO operation, written as
                    // a URI with an empty path. Default ports are omitted and any
                    // path component is ignored."
                    //
                    // This leaves it unclear as to whether the path must be empty,
                    // or whether it can be non-empty and will be ignored. To be on
                    // the safe side, use a proper web origin (with empty path).
                    &CONFIG.domain_origin(),
                    "ios:bundle-id:com.8bit.bitwarden",
                    "android:apk-key-hash:dUGFzUzf3lmHSLBDBIv+WaFyZMI" ]
                }]
            })),
        ),
        true,
    )
}

#[get("/.well-known/apple-app-site-association")]
fn apple_app_site_association() -> Cached<(ContentType, Json<Value>)> {
    Cached::long(
        (
            ContentType::JSON,
            Json(json!({
                "webcredentials": {
                    "apps": [
                        "LTZ2PFU5D6.com.8bit.bitwarden",
                        "LTZ2PFU5D6.com.8bit.bitwarden.beta"
                    ]
                }
            })),
        ),
        true,
    )
}

#[get("/<p..>", rank = 10)] // Only match this if the other routes don't match
async fn web_files(p: PathBuf) -> Cached<Option<NamedFile>> {
    Cached::long(NamedFile::open(Path::new(&CONFIG.web_vault_folder()).join(p)).await.ok(), true)
}

#[get("/attachments/<cipher_id>/<file_id>?<token>")]
async fn attachments(cipher_id: CipherId, file_id: AttachmentId, token: String) -> Option<NamedFile> {
    let Ok(claims) = decode_file_download(&token) else {
        return None;
    };
    if claims.sub != cipher_id || claims.file_id != file_id {
        return None;
    }

    NamedFile::open(Path::new(&CONFIG.attachments_folder()).join(cipher_id.as_ref()).join(file_id.as_ref())).await.ok()
}

// We use DbConn here to let the alive healthcheck also verify the database connection.
#[get("/alive")]
fn alive(_conn: DbConn) -> Json<String> {
    now()
}

#[head("/alive")]
fn alive_head(_conn: DbConn) -> EmptyResult {
    // Avoid logging spurious "No matching routes for HEAD /alive" errors
    // due to <https://github.com/SergioBenitez/Rocket/issues/1098>.
    Ok(())
}

// Health checks in the "Health Check Response Format for HTTP APIs" draft
// (https://datatracker.ietf.org/doc/html/draft-inadarei-api-health-check):
// `application/health+json`, `status` "pass" or "fail", 200 or 503.
type HealthResponse = (Status, (ContentType, Json<Value>));

fn health(pass: bool, checks: Option<Value>) -> HealthResponse {
    let mut body = json!({ "status": if pass { "pass" } else { "fail" } });
    if let Some(checks) = checks {
        body["checks"] = checks;
    }
    let status = if pass {
        Status::Ok
    } else {
        Status::ServiceUnavailable
    };
    (status, (ContentType::new("application", "health+json"), Json(body)))
}

// Health routes are also mounted at root when DOMAIN includes a base path.
pub fn homestack_health_routes() -> Vec<Route> {
    routes![health_live, health_ready]
}

#[get("/health/live")]
fn health_live(heartbeat: &State<Arc<crate::homestack_health::Heartbeat>>) -> HealthResponse {
    let snapshot = heartbeat.snapshot();
    health(snapshot.pass, Some(snapshot.checks()))
}

#[get("/health/ready")]
async fn health_ready(
    pool: &State<DbPool>,
    heartbeat: &State<Arc<crate::homestack_health::Heartbeat>>,
) -> HealthResponse {
    let snapshot = heartbeat.snapshot();
    if !snapshot.pass {
        return health(false, Some(snapshot.checks()));
    }
    let database =
        tokio::time::timeout(std::time::Duration::from_secs(2), crate::homestack_health::database_ready(pool))
            .await
            .unwrap_or(false);
    let snapshot = heartbeat.snapshot();
    let mut checks = snapshot.checks();
    checks["database:connection"] = json!([{ "status": if database { "pass" } else { "fail" } }]);
    health(snapshot.pass && database, Some(checks))
}

// A separate probe avoids making heartbeat recovery depend on its own freshness.
#[get("/health/heartbeat")]
async fn heartbeat_probe(pool: &State<DbPool>) -> Status {
    if crate::homestack_health::database_ready(pool).await {
        Status::NoContent
    } else {
        Status::ServiceUnavailable
    }
}

pub fn homestack_status_routes() -> Vec<Route> {
    routes![homestack_status, heartbeat_probe]
}

#[derive(Responder)]
#[response(content_type = "json")]
struct HomestackStatus(Json<crate::homestack_status::Report>, Header<'static>);

#[get("/.well-known/homestack/status")]
async fn homestack_status(
    telemetry: &State<crate::homestack_status::Telemetry>,
    heartbeat: &State<Arc<crate::homestack_health::Heartbeat>>,
) -> HomestackStatus {
    let mut report = telemetry.report(crate::homestack_status::Status::Pass).await;
    report.status = heartbeat.snapshot().status();
    HomestackStatus(Json(report), Header::new("Cache-Control", "no-store"))
}

// This endpoint/function is used during development and development only.
// It allows to easily develop the admin interface by always loading the files from disk instead from a slice of bytes
// This will only be active during a debug build and only when `RELOAD_TEMPLATES` is set to `true`
// NOTE: Do not forget to add any new files added to the `static_files` function below!
#[cfg(debug_assertions)]
#[get("/vw_static/<filename>", rank = 1)]
pub async fn static_files_dev(filename: PathBuf) -> Option<NamedFile> {
    warn!("LOADING STATIC FILES FROM DISK");
    let file = filename.to_str().unwrap_or_default();
    let ext = filename.extension().unwrap_or_default();

    let path = if ext == "png" || ext == "svg" {
        tokio::fs::canonicalize(Path::new(file!()).parent().unwrap().join("../static/images/").join(file)).await
    } else {
        tokio::fs::canonicalize(Path::new(file!()).parent().unwrap().join("../static/scripts/").join(file)).await
    };

    if let Ok(path) = path {
        return NamedFile::open(path).await.ok();
    }
    None
}

#[get("/vw_static/<filename>", rank = 2)]
pub fn static_files(filename: &str) -> Result<(ContentType, &'static [u8]), Error> {
    match filename {
        "404.png" => Ok((ContentType::PNG, include_bytes!("../static/images/404.png"))),
        "mail-github.png" => Ok((ContentType::PNG, include_bytes!("../static/images/mail-github.png"))),
        "logo-gray.png" => Ok((ContentType::PNG, include_bytes!("../static/images/logo-gray.png"))),
        "error-x.svg" => Ok((ContentType::SVG, include_bytes!("../static/images/error-x.svg"))),
        "hibp.png" => Ok((ContentType::PNG, include_bytes!("../static/images/hibp.png"))),
        "vaultwarden-icon.png" => Ok((ContentType::PNG, include_bytes!("../static/images/vaultwarden-icon.png"))),
        "vaultwarden-favicon.png" => Ok((ContentType::PNG, include_bytes!("../static/images/vaultwarden-favicon.png"))),
        "404.css" => Ok((ContentType::CSS, include_bytes!("../static/scripts/404.css"))),
        "admin.css" => Ok((ContentType::CSS, include_bytes!("../static/scripts/admin.css"))),
        "admin.js" => Ok((ContentType::JavaScript, include_bytes!("../static/scripts/admin.js"))),
        "admin_settings.js" => Ok((ContentType::JavaScript, include_bytes!("../static/scripts/admin_settings.js"))),
        "admin_users.js" => Ok((ContentType::JavaScript, include_bytes!("../static/scripts/admin_users.js"))),
        "admin_organizations.js" => {
            Ok((ContentType::JavaScript, include_bytes!("../static/scripts/admin_organizations.js")))
        }
        "admin_diagnostics.js" => {
            Ok((ContentType::JavaScript, include_bytes!("../static/scripts/admin_diagnostics.js")))
        }
        "bootstrap.css" => Ok((ContentType::CSS, include_bytes!("../static/scripts/bootstrap.css"))),
        "bootstrap.bundle.js" => Ok((ContentType::JavaScript, include_bytes!("../static/scripts/bootstrap.bundle.js"))),
        "jdenticon-3.3.0.js" => Ok((ContentType::JavaScript, include_bytes!("../static/scripts/jdenticon-3.3.0.js"))),
        "datatables.js" => Ok((ContentType::JavaScript, include_bytes!("../static/scripts/datatables.js"))),
        "datatables.css" => Ok((ContentType::CSS, include_bytes!("../static/scripts/datatables.css"))),
        _ => err!(format!("Static file not found: {filename}")),
    }
}

#[cfg(all(test, sqlite))]
mod homestack_tests {
    use super::*;
    use crate::{homestack_health::Heartbeat, homestack_status::Telemetry};
    use rocket::local::asynchronous::Client;
    use std::time::{Duration, Instant};

    async fn client(started: Instant) -> Client {
        Client::tracked(
            rocket::build()
                .mount("/vault", routes![health_live, health_ready])
                .mount("/", homestack_health_routes())
                .mount("/", homestack_status_routes())
                .manage(Arc::new(Heartbeat::new(started, Duration::from_secs(10))))
                .manage(Telemetry::start(started).unwrap())
                .manage(DbPool::homestack_test_pool()),
        )
        .await
        .unwrap()
    }

    #[rocket::async_test]
    async fn homestack_health_is_available_at_root_and_basepath() {
        let client = client(Instant::now()).await;
        for path in ["/health/live", "/health/ready", "/vault/health/live", "/vault/health/ready"] {
            let response = client.get(path).dispatch().await;
            assert_eq!(response.status(), Status::Ok, "{path}");
            assert_eq!(response.content_type(), Some(ContentType::new("application", "health+json")));
            let body = response.into_json::<Value>().await.unwrap();
            assert_eq!(body["status"], "pass");
            assert_eq!(body["checks"]["heartbeat:count"][0]["observedValue"], 0);
            assert_eq!(body["checks"]["heartbeat:age"][0]["observedUnit"], "s");
        }
        assert_eq!(client.get("/health/heartbeat").dispatch().await.status(), Status::NoContent);
        let response = client.get("/.well-known/homestack/status").dispatch().await;
        assert_eq!(response.headers().get_one("Cache-Control"), Some("no-store"));
        assert_eq!(response.content_type(), Some(ContentType::JSON));
        let body = response.into_json::<Value>().await.unwrap();
        assert_eq!(body["schema"], 2);
        assert_eq!(body["status"], "pass");
        assert!(body["sample_uptime_usec"].is_u64());
    }

    #[rocket::async_test]
    async fn homestack_stale_heartbeat_fails_health_and_telemetry_but_probe_can_recover() {
        let client = client(Instant::now() - Duration::from_secs(31)).await;
        for path in ["/health/live", "/health/ready"] {
            let response = client.get(path).dispatch().await;
            assert_eq!(response.status(), Status::ServiceUnavailable);
            let body = response.into_json::<Value>().await.unwrap();
            assert_eq!(body["status"], "fail");
            assert_eq!(body["checks"]["heartbeat:count"][0]["observedValue"], 0);
        }
        let response = client.get("/.well-known/homestack/status").dispatch().await;
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_json::<Value>().await.unwrap()["status"], "fail");
        // Heartbeat probes remain independent of freshness, avoiding a recovery deadlock.
        assert_eq!(client.get("/health/heartbeat").dispatch().await.status(), Status::NoContent);
    }

    #[rocket::async_test]
    async fn homestack_readiness_times_out_when_database_pool_is_exhausted() {
        let client = client(Instant::now()).await;
        let pool = client.rocket().state::<DbPool>().unwrap();
        let held = pool.get().await.unwrap();
        let before = Instant::now();
        let response = client.get("/health/ready").dispatch().await;
        assert!(before.elapsed() < Duration::from_secs(3));
        assert_eq!(response.status(), Status::ServiceUnavailable);
        let body = response.into_json::<Value>().await.unwrap();
        assert_eq!(body["checks"]["database:connection"][0]["status"], "fail");
        assert_eq!(client.get("/health/live").dispatch().await.status(), Status::Ok);
        drop(held);
        assert_eq!(client.get("/health/ready").dispatch().await.status(), Status::Ok);
    }
}
