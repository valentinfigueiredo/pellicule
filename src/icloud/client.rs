//! Le dialogue avec les serveurs d'Apple : connexion (SRP + code à 6 chiffres),
//! session mémorisée, puis lecture de la photothèque iCloud (CloudKit).

use super::srp;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const AUTH: &str = "https://idmsa.apple.com/appleauth/auth";
const AUTH_ROOT: &str = "https://idmsa.apple.com";
const HOME: &str = "https://www.icloud.com";
const SETUP: &str = "https://setup.icloud.com/setup/ws/1";
const WIDGET_KEY: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// En-têtes renvoyés par Apple qu'il faut garder d'une requête à l'autre.
const KEPT_HEADERS: [(&str, &str); 6] = [
    ("x-apple-id-account-country", "account_country"),
    ("x-apple-id-session-id", "session_id"),
    ("x-apple-session-token", "session_token"),
    ("x-apple-twosv-trust-token", "trust_token"),
    ("scnt", "scnt"),
    ("x-apple-i-rscd", "apple_rscd"),
];

#[derive(Debug)]
pub enum Error {
    /// La session n'est plus valable : il faut se reconnecter.
    NeedLogin(String),
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NeedLogin(m) | Error::Other(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn other(e: impl std::fmt::Display) -> Error {
    Error::Other(e.to_string())
}

pub enum Login {
    Ready,
    /// Apple a envoyé un code à 6 chiffres sur les appareils de confiance.
    NeedCode,
}

#[derive(Clone)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Le message d'erreur d'Apple, s'il y en a un.
    fn error(&self) -> String {
        let j = self.json();
        let pick = |v: &Value| v.as_str().map(str::to_string);
        j.pointer("/serviceErrors/0/message")
            .and_then(pick)
            .or_else(|| j.pointer("/service_errors/0/message").and_then(pick))
            .or_else(|| j.get("errorMessage").and_then(pick))
            .or_else(|| j.get("reason").and_then(pick))
            .or_else(|| j.get("serverErrorCode").and_then(pick))
            .or_else(|| j.get("error").and_then(pick))
            .unwrap_or_else(|| format!("erreur {}", self.status))
    }
}

/// Une photo ou vidéo de la photothèque iCloud.
pub struct Asset {
    pub id: String,
    pub filename: String,
    pub size: u64,
    pub url: String,
    /// Date de prise de vue, en millisecondes depuis 1970.
    pub date_ms: i64,
}

pub struct Session {
    dir: PathBuf,
    agent: ureq::Agent,
    pub apple_id: String,
    client_id: String,
    data: HashMap<String, String>,
    cookies: Vec<Cookie>,
    account: Value,
}

impl Session {
    /// Reprend la session enregistrée dans `dir` (ou en crée une vide).
    pub fn load(dir: &Path) -> Session {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(90)))
            .build()
            .into();
        let mut s = Session {
            dir: dir.to_path_buf(),
            agent,
            apple_id: String::new(),
            client_id: format!("auth-{}", uuid()),
            data: HashMap::new(),
            cookies: Vec::new(),
            account: Value::Null,
        };
        let stored = fs::read(dir.join("session.bin")).ok().and_then(|b| crate::platform::unprotect(&b));
        if let Some(v) = stored.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            s.apple_id = text("apple_id");
            if !text("client_id").is_empty() {
                s.client_id = text("client_id");
            }
            if let Some(data) = v.get("data").and_then(Value::as_object) {
                s.data = data.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
            }
            if let Some(cookies) = v.get("cookies").and_then(Value::as_array) {
                s.cookies = cookies
                    .iter()
                    .filter_map(|c| {
                        Some(Cookie {
                            name: c.get("name")?.as_str()?.to_string(),
                            value: c.get("value")?.as_str()?.to_string(),
                            domain: c.get("domain")?.as_str()?.to_string(),
                            host_only: c.get("host_only")?.as_bool()?,
                        })
                    })
                    .collect();
            }
        }
        s
    }

    /// Enregistre la session (chiffrée par Windows pour ton compte utilisateur).
    fn save(&self) {
        let cookies: Vec<Value> = self
            .cookies
            .iter()
            .map(|c| json!({"name": c.name, "value": c.value, "domain": c.domain, "host_only": c.host_only}))
            .collect();
        let v = json!({"apple_id": self.apple_id, "client_id": self.client_id, "data": self.data, "cookies": cookies});
        if fs::create_dir_all(&self.dir).is_ok() {
            let bytes = crate::platform::protect(v.to_string().as_bytes());
            let tmp = self.dir.join("session.bin.tmp");
            if fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(tmp, self.dir.join("session.bin"));
            }
        }
    }

    pub fn forget(&mut self) {
        self.data.clear();
        self.cookies.clear();
        self.account = Value::Null;
        let _ = fs::remove_file(self.dir.join("session.bin"));
    }

    // ---------------------------------------------------------------- HTTP

    fn request(&mut self, method: &str, url: &str, extra: &[(&str, String)], body: Option<String>) -> Result<Reply> {
        let host = host_of(url);
        let mut headers: Vec<(String, String)> = vec![
            ("Origin".into(), HOME.into()),
            ("Referer".into(), format!("{HOME}/")),
            ("User-Agent".into(), USER_AGENT.into()),
        ];
        for (k, v) in extra {
            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            headers.push((k.to_string(), v.clone()));
        }
        let cookie: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| c.domain == host || (!c.host_only && host.ends_with(&format!(".{}", c.domain))))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if !cookie.is_empty() {
            headers.push(("Cookie".into(), cookie.join("; ")));
        }

        let mut builder = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in &headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let resp = match body {
            Some(b) => self.agent.run(builder.body(b).map_err(other)?),
            None => self.agent.run(builder.body(()).map_err(other)?),
        }
        .map_err(|e| Error::Other(format!("connexion à Apple impossible : {e}")))?;

        let status = resp.status().as_u16();
        self.trace(method, url, status);
        for (name, value) in resp.headers() {
            let Ok(value) = value.to_str() else { continue };
            let name = name.as_str();
            if name == "set-cookie" {
                self.store_cookie(value, &host);
            } else if let Some((_, key)) = KEPT_HEADERS.iter().find(|(h, _)| *h == name) {
                if !value.is_empty() {
                    self.data.insert(key.to_string(), value.to_string());
                }
            }
        }
        let body = resp.into_body().with_config().limit(64 << 20).read_to_string().unwrap_or_default();
        self.save();
        Ok(Reply { status, body })
    }

    /// Journal technique (`.pellicule/icloud/journal.log`) : méthode, adresse sans paramètres, statut.
    /// Jamais de mot de passe, de jeton ni de contenu : de quoi comprendre une panne, rien de plus.
    fn trace(&self, method: &str, url: &str, status: u16) {
        let path = url.split('?').next().unwrap_or(url);
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = fs::create_dir_all(&self.dir);
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(self.dir.join("journal.log")) {
            let _ = writeln!(f, "{now}\t{method}\t{path}\t{status}");
        }
    }

    fn store_cookie(&mut self, header: &str, host: &str) {
        let mut parts = header.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.trim().split_once('=')) else { return };
        let (mut domain, mut host_only, mut expired) = (host.to_string(), true, false);
        for attr in parts {
            let (k, v) = attr.trim().split_once('=').unwrap_or((attr.trim(), ""));
            match k.to_ascii_lowercase().as_str() {
                "domain" if !v.is_empty() => {
                    domain = v.trim_start_matches('.').to_ascii_lowercase();
                    host_only = false;
                }
                "max-age" => expired |= v.trim().parse::<i64>().is_ok_and(|n| n <= 0),
                "expires" => expired |= v.contains("1970"),
                _ => {}
            }
        }
        self.cookies.retain(|c| !(c.name == name && c.domain == domain));
        if !expired {
            self.cookies.push(Cookie { name: name.into(), value: value.into(), domain, host_only });
        }
    }

    fn auth_headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![
            ("Accept", "application/json, text/javascript".to_string()),
            ("Content-Type", "application/json".to_string()),
            ("X-Apple-OAuth-Client-Id", WIDGET_KEY.to_string()),
            ("X-Apple-OAuth-Client-Type", "firstPartyAuth".to_string()),
            ("X-Apple-OAuth-Redirect-URI", HOME.to_string()),
            ("X-Apple-OAuth-Require-Grant-Code", "true".to_string()),
            ("X-Apple-OAuth-Response-Mode", "web_message".to_string()),
            ("X-Apple-OAuth-Response-Type", "code".to_string()),
            ("X-Apple-OAuth-State", self.client_id.clone()),
            ("X-Apple-Widget-Key", WIDGET_KEY.to_string()),
        ];
        if let Some(v) = self.data.get("scnt") {
            h.push(("scnt", v.clone()));
        }
        if let Some(v) = self.data.get("session_id") {
            h.push(("X-Apple-ID-Session-Id", v.clone()));
        }
        h
    }

    fn auth_root_headers(&self) -> Vec<(&'static str, String)> {
        let mut h = self.auth_headers();
        h.push(("Origin", AUTH_ROOT.to_string()));
        h.push(("Referer", format!("{AUTH_ROOT}/")));
        h
    }

    // ---------------------------------------------------------------- connexion

    /// Reprend la session enregistrée, sans mot de passe. `false` s'il faut se reconnecter.
    pub fn resume(&mut self) -> Result<bool> {
        if !self.data.contains_key("session_token") {
            return Ok(false);
        }
        let r = self.request("POST", &format!("{SETUP}/validate"), &[], Some("null".into()))?;
        if r.ok() && r.json().get("dsInfo").is_some() {
            self.account = r.json();
        } else if self.account_login().is_err() {
            return Ok(false);
        }
        Ok(!self.requires_code())
    }

    pub fn login(&mut self, apple_id: &str, password: &str) -> Result<Login> {
        let apple_id = apple_id.trim().to_lowercase();
        if apple_id != self.apple_id {
            // Autre compte : on repart de zéro.
            self.forget();
            self.apple_id = apple_id.clone();
        }
        // Apple redemande un code si on lui renvoie une vieille session : on ne garde que la confiance.
        let trust = self.data.get("trust_token").cloned();
        self.data.retain(|k, _| k == "trust_token");

        let client = srp::Client::new(&apple_id);
        let b64 = base64::engine::general_purpose::STANDARD;
        let init = json!({"a": b64.encode(client.public()), "accountName": apple_id, "protocols": ["s2k", "s2k_fo"]});
        let r = self.request("POST", &format!("{AUTH}/signin/init"), &self.auth_root_headers(), Some(init.to_string()))?;
        if !r.ok() {
            return Err(login_error(&r));
        }
        let challenge = r.json();
        let field = |k: &str| challenge.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let salt = b64.decode(field("salt")).map_err(|_| Error::Other("réponse d'Apple illisible".into()))?;
        let server_b = b64.decode(field("b")).map_err(|_| Error::Other("réponse d'Apple illisible".into()))?;
        let iterations = challenge.get("iteration").and_then(Value::as_u64).unwrap_or(0) as u32;
        let key = srp::password_key(password, &field("protocol"), &salt, iterations);
        let proof = client.prove(&key, &salt, &server_b).ok_or_else(|| Error::Other("réponse d'Apple invalide".into()))?;

        let complete = json!({
            "accountName": apple_id,
            "c": field("c"),
            "m1": b64.encode(&proof.m1),
            "m2": b64.encode(&proof.m2),
            "rememberMe": true,
            "trustTokens": trust.iter().collect::<Vec<_>>(),
        });
        let url = format!("{AUTH}/signin/complete?isRememberMeEnabled=true");
        let r = self.request("POST", &url, &self.auth_root_headers(), Some(complete.to_string()))?;
        match r.status {
            200..=299 | 409 => {}
            // Compte sans double authentification : Apple demande une « réparation ».
            412 => {
                let h = self.auth_headers();
                self.request("POST", &format!("{AUTH}/repair/complete"), &h, Some("{}".into()))?;
            }
            _ => return Err(login_error(&r)),
        }

        self.account_login()?;
        if self.requires_code() {
            // Depuis 2026, Apple attend cette requête pour envoyer le code sur l'iPhone.
            let mut h = self.auth_headers();
            h.push(("Accept", "application/json".into()));
            let _ = self.request("PUT", &format!("{AUTH}/verify/trusteddevice/securitycode"), &h, None);
            return Ok(Login::NeedCode);
        }
        Ok(Login::Ready)
    }

    pub fn submit_code(&mut self, code: &str) -> Result<()> {
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        let mut h = self.auth_headers();
        h.push(("Accept", "application/json".into()));
        let body = json!({"securityCode": {"code": code}}).to_string();
        let r = self.request("POST", &format!("{AUTH}/verify/trusteddevice/securitycode"), &h, Some(body))?;
        if !r.ok() {
            return Err(Error::Other(if r.status == 400 || r.body.contains("-21669") {
                "Code incorrect, réessaie.".into()
            } else {
                format!("Code refusé : {}", r.error())
            }));
        }
        // On demande à Apple de faire confiance à Pellicule (plus de code pendant un moment).
        let h = self.auth_headers();
        self.request("GET", &format!("{AUTH}/2sv/trust"), &h, None)?;
        self.account_login()?;
        if self.requires_code() {
            return Err(Error::Other("Apple demande encore un code, réessaie.".into()));
        }
        Ok(())
    }

    fn account_login(&mut self) -> Result<()> {
        let get = |k: &str| self.data.get(k).cloned().unwrap_or_default();
        let body = json!({
            "accountCountryCode": get("account_country"),
            "dsWebAuthToken": get("session_token"),
            "extended_login": true,
            "trustToken": get("trust_token"),
        });
        let r = self.request("POST", &format!("{SETUP}/accountLogin"), &[], Some(body.to_string()))?;
        if !r.ok() {
            return Err(Error::NeedLogin(format!("Connexion refusée : {}", r.error())));
        }
        let account = r.json();
        if account.get("domainToUse").is_some() {
            return Err(Error::Other("Les comptes iCloud chinois ne sont pas pris en charge.".into()));
        }
        self.account = account;
        Ok(())
    }

    fn requires_code(&self) -> bool {
        let ds = &self.account["dsInfo"];
        ds["hsaVersion"].as_i64().unwrap_or(0) == 2
            && (self.account["hsaChallengeRequired"].as_bool().unwrap_or(false)
                || !self.account["hsaTrustedBrowser"].as_bool().unwrap_or(false))
            && ds["hasICloudQualifyingDevice"].as_bool().unwrap_or(false)
    }

    // ---------------------------------------------------------------- photothèque

    fn photos_url(&self, action: &str) -> Result<String> {
        let root = self.account.pointer("/webservices/ckdatabasews/url").and_then(Value::as_str).ok_or_else(|| {
            Error::Other("iCloud Photos n'est pas accessible : vérifie qu'il est activé sur ton iPhone.".into())
        })?;
        let dsid = self.account.pointer("/dsInfo/dsid").and_then(Value::as_str).unwrap_or_default();
        Ok(format!(
            "{root}/database/1/com.apple.photos.cloud/production/private/{action}?clientBuildNumber=2522Project44&clientMasteringNumber=2522B2&clientId={}&dsid={dsid}&remapEnums=True&getCurrentSyncToken=True",
            self.client_id
        ))
    }

    fn cloudkit(&mut self, action: &str, body: Value) -> Result<Value> {
        let url = self.photos_url(action)?;
        let r = self.request("POST", &url, &[("Content-Type", "text/plain".into())], Some(body.to_string()))?;
        let j = r.json();
        let code = j.get("serverErrorCode").and_then(Value::as_str).unwrap_or_default();
        if matches!(r.status, 401 | 421 | 450) || code == "AUTHENTICATION_FAILED" {
            return Err(Error::NeedLogin("Session iCloud expirée, reconnecte-toi.".into()));
        }
        if code == "ACCESS_DENIED" || r.status == 403 {
            return Err(Error::Other(ACCESS_DENIED.into()));
        }
        if code == "ZONE_NOT_FOUND" {
            return Err(Error::Other("iCloud Photos n'est pas encore configuré sur ce compte (va une fois sur icloud.com).".into()));
        }
        if !r.ok() {
            return Err(Error::Other(format!("iCloud Photos : {}", r.error())));
        }
        Ok(j)
    }

    /// Nombre de photos et vidéos de la photothèque.
    pub fn count(&mut self) -> Result<u64> {
        let state = self.cloudkit(
            "records/query",
            json!({"query": {"recordType": "CheckIndexingState"}, "zoneID": {"zoneName": "PrimarySync"}}),
        )?;
        if state.pointer("/records/0/fields/state/value").and_then(Value::as_str) != Some("FINISHED") {
            return Err(Error::Other("iCloud prépare encore ta photothèque, réessaie dans quelques minutes.".into()));
        }
        let r = self.cloudkit(
            "internal/records/query/batch",
            json!({"batch": [{
                "resultsLimit": 1,
                "query": {
                    "filterBy": {"fieldName": "indexCountID", "fieldValue": {"type": "STRING_LIST", "value": ["CPLAssetByAssetDateWithoutHiddenOrDeleted"]}, "comparator": "IN"},
                    "recordType": "HyperionIndexCountLookup"
                },
                "zoneWide": true,
                "zoneID": {"zoneName": "PrimarySync"}
            }]}),
        )?;
        Ok(r.pointer("/batch/0/records/0/fields/itemCount/value").and_then(Value::as_u64).unwrap_or(0))
    }

    /// Une page de la photothèque (du plus ancien au plus récent) à partir de `offset`.
    pub fn page(&mut self, offset: u64) -> Result<Vec<Asset>> {
        let r = self.cloudkit(
            "records/query",
            json!({
                "query": {
                    "filterBy": [
                        {"fieldName": "startRank", "fieldValue": {"type": "INT64", "value": offset}, "comparator": "EQUALS"},
                        {"fieldName": "direction", "fieldValue": {"type": "STRING", "value": "ASCENDING"}, "comparator": "EQUALS"}
                    ],
                    "recordType": "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"
                },
                "resultsLimit": 200,
                "desiredKeys": ["resOriginalRes", "resOriginalFileType", "itemType", "filenameEnc", "recordName",
                                "recordType", "masterRef", "assetDate", "addedDate", "isDeleted", "isHidden"],
                "zoneID": {"zoneName": "PrimarySync"}
            }),
        )?;
        let records = r["records"].as_array().cloned().unwrap_or_default();
        let dates: HashMap<&str, i64> = records
            .iter()
            .filter(|rec| rec["recordType"] == "CPLAsset")
            .filter_map(|rec| {
                let master = rec.pointer("/fields/masterRef/value/recordName")?.as_str()?;
                Some((master, rec.pointer("/fields/assetDate/value").and_then(Value::as_i64).unwrap_or(0)))
            })
            .collect();
        Ok(records
            .iter()
            .filter(|rec| rec["recordType"] == "CPLMaster")
            .filter_map(|m| {
                let id = m["recordName"].as_str()?.to_string();
                let res = m.pointer("/fields/resOriginalRes/value")?;
                let filename = filename_of(m).unwrap_or_else(|| format!("{}.{}", &id[..id.len().min(12)], ext_for(m)));
                Some(Asset {
                    filename,
                    size: res["size"].as_u64().unwrap_or(0),
                    url: res["downloadURL"].as_str()?.to_string(),
                    date_ms: dates.get(id.as_str()).copied().unwrap_or(0),
                    id,
                })
            })
            .collect())
    }

    /// Télécharge un original dans `dest`.
    pub fn download(&self, url: &str, dest: &Path) -> Result<u64> {
        let resp = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| Error::Other(format!("téléchargement impossible : {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!("téléchargement refusé ({})", resp.status())));
        }
        let mut out = io::BufWriter::new(fs::File::create(dest).map_err(other)?);
        let n = io::copy(&mut resp.into_body().into_reader(), &mut out).map_err(other)?;
        out.flush().map_err(other)?;
        Ok(n)
    }
}

pub const ACCESS_DENIED: &str = "Apple refuse l'accès. Sur ton iPhone : Réglages > ton nom > iCloud : active « Accéder aux données iCloud sur le web » et désactive « Protection avancée des données ».";

fn login_error(r: &Reply) -> Error {
    match r.status {
        401 | 403 => Error::Other("Identifiant Apple ou mot de passe incorrect.".into()),
        503 => Error::Other("Apple limite les connexions pour le moment, réessaie dans quelques minutes.".into()),
        _ => Error::Other(format!("Connexion refusée : {}", r.error())),
    }
}

fn filename_of(master: &Value) -> Option<String> {
    let enc = master.pointer("/fields/filenameEnc")?;
    let value = enc["value"].as_str()?;
    let name = match enc["type"].as_str()? {
        "STRING" => value.to_string(),
        _ => String::from_utf8(base64::engine::general_purpose::STANDARD.decode(value).ok()?).ok()?,
    };
    // Pas de chemin ni de caractère interdit sous Windows.
    let clean: String = name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    (!clean.trim().is_empty()).then_some(clean)
}

fn ext_for(master: &Value) -> &'static str {
    match master.pointer("/fields/itemType/value").and_then(Value::as_str).unwrap_or("") {
        "public.heic" | "public.heif" => "heic",
        "public.png" => "png",
        "com.apple.quicktime-movie" => "mov",
        "public.mpeg-4" => "mp4",
        _ => "jpg",
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', ':', '?']).next().unwrap_or("").to_ascii_lowercase()
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies() {
        let mut s = Session::load(Path::new("/nonexistent-pellicule-test"));
        s.store_cookie("X-APPLE-WEBAUTH-TOKEN=abc; Domain=.icloud.com; Path=/; Secure; HttpOnly", "setup.icloud.com");
        s.store_cookie("aasp=xyz; Path=/; Secure", "idmsa.apple.com");
        assert_eq!(s.cookies.len(), 2);
        assert!(s.cookies.iter().any(|c| c.domain == "icloud.com" && !c.host_only));
        s.store_cookie("X-APPLE-WEBAUTH-TOKEN=; Domain=.icloud.com; Expires=Thu, 01-Jan-1970 00:00:01 GMT", "setup.icloud.com");
        assert_eq!(s.cookies.len(), 1);
        assert_eq!(host_of("https://p42-ckdatabasews.icloud.com:443/database/1"), "p42-ckdatabasews.icloud.com");
    }

    #[test]
    fn noms() {
        let enc = base64::engine::general_purpose::STANDARD.encode("IMG_1234.HEIC");
        let m = json!({"recordName": "AbC", "fields": {"filenameEnc": {"type": "ENCRYPTED_BYTES", "value": enc}}});
        assert_eq!(filename_of(&m).as_deref(), Some("IMG_1234.HEIC"));
        let m = json!({"fields": {"filenameEnc": {"type": "STRING", "value": "a/b:c.jpg"}}});
        assert_eq!(filename_of(&m).as_deref(), Some("a_b_c.jpg"));
        assert_eq!(uuid().len(), 36);
    }

    /// Parle au vrai serveur d'Apple avec un compte inventé : `cargo test -- --ignored apple`.
    /// Vérifie que les requêtes sont acceptées et que l'erreur est bien comprise.
    #[test]
    #[ignore]
    fn apple_refuse_un_compte_invente() {
        let dir = std::env::temp_dir().join("pellicule-test-icloud");
        let mut s = Session::load(&dir);
        match s.login("pellicule.test.inexistant@example.com", "mauvais-mot-de-passe") {
            Err(Error::Other(m)) => println!("refusé comme prévu : {m}"),
            Err(e) => panic!("erreur inattendue : {e}"),
            Ok(_) => panic!("connexion acceptée ?!"),
        }
        print!("{}", fs::read_to_string(dir.join("journal.log")).unwrap_or_default());
        let _ = fs::remove_dir_all(dir);
    }
}
