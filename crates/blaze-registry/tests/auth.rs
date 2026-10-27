//! A registry that requires a token, served from memory: `config.json` asked first with no
//! token and again with it after a 401, the index and the `.crate` files asked with it, and a
//! missing or refused token said as cargo says it.

use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use blaze_registry::{
    Context, Locked, Package, SourceConfig, Sources,
    auth::{Auth, Shell},
    download::checksum,
    http::{Http, Response},
};
use blaze_source::SourceId;
use rustc_hash::FxHashMap as HashMap;
use semver::Version;

const BASE: &str = "https://private.example.com/";
const DL: &str = "https://dl.private.example.com/";
const LOGIN: &str = "https://private.example.com/me";

/// Files by URL, every one of which wants `authorization: <token>`, and what was asked.
struct Server {
    token: String,
    files: Mutex<HashMap<String, Vec<u8>>>,
    asked: Mutex<Vec<(String, Option<String>)>>,
}

impl Http for Server {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        let sent = headers.iter().find(|(k, _)| *k == "authorization").map(|(_, v)| v.to_string());
        self.asked.lock().unwrap().push((url.to_owned(), sent.clone()));
        if sent.as_deref() != Some(self.token.as_str()) {
            return Ok(Response {
                status: 401,
                headers: vec![(
                    "www-authenticate".to_owned(),
                    format!(r#"Basic realm="x", Cargo login_url="{LOGIN}""#),
                )],
                body: b"who are you".to_vec(),
            });
        }
        match self.files.lock().unwrap().get(url) {
            Some(body) => Ok(Response { status: 200, headers: vec![], body: body.clone() }),
            None => Ok(Response { status: 404, ..Response::default() }),
        }
    }
}

#[derive(Clone, Default)]
struct Said(Arc<Mutex<Vec<String>>>);

impl Shell for Said {
    fn warn(&self, text: &str) {
        self.0.lock().unwrap().push(format!("warning: {text}"));
    }
    fn note(&self, text: &str) {
        self.0.lock().unwrap().push(format!("note: {text}"));
    }
    fn status(&self, word: &str, text: &str) {
        self.0.lock().unwrap().push(format!("{word} {text}"));
    }
    fn verbose(&self) -> bool {
        false
    }
}

fn tarball(name: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let data = b"[package]\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut header, format!("{name}-1.0.0/Cargo.toml"), &data[..]).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

struct Fixture {
    server: Arc<Server>,
    home: PathBuf,
    said: Said,
    package: Package,
}

impl Fixture {
    /// A registry `private` at [`BASE`] wanting `token`, with `config` as
    /// `$CARGO_HOME/config.toml` and `credentials` as `credentials.toml`.
    fn new(name: &str, token: &str, config: &str, credentials: &str) -> Fixture {
        let home = std::env::temp_dir().join(format!("blaze-auth-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = format!("[registries.private]\nindex = \"sparse+{BASE}\"\n{config}");
        std::fs::write(home.join("config.toml"), config).unwrap();
        if !credentials.is_empty() {
            std::fs::write(home.join("credentials.toml"), credentials).unwrap();
        }
        let bytes = tarball("secret");
        let sum = checksum(&bytes);
        let files = HashMap::from_iter([
            (
                format!("{BASE}config.json"),
                format!(r#"{{"dl":"{DL}{{crate}}/{{version}}","auth-required":true}}"#).into_bytes(),
            ),
            (
                format!("{BASE}se/cr/secret"),
                format!(r#"{{"name":"secret","vers":"1.0.0","deps":[],"cksum":"{sum}","features":{{}}}}"#)
                    .into_bytes(),
            ),
            (format!("{DL}secret/1.0.0"), bytes),
        ]);
        let server = Arc::new(Server {
            token: token.to_owned(),
            files: Mutex::new(files),
            asked: Mutex::default(),
        });
        let package =
            Package { name: "secret".to_owned(), version: Version::new(1, 0, 0), checksum: sum };
        Fixture { server, home, said: Said::default(), package }
    }

    fn sources(&self, env: &[(&str, &str)]) -> Sources {
        let mut vars = vec![("CARGO_HOME".to_owned(), self.home.display().to_string())];
        vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let config = blaze_manifest::Config::load(&self.home, vars, &[]).unwrap();
        let mut context = Context::new(self.home.clone(), self.server.clone());
        context.net.retry.quiet = true;
        context.auth = Some(Arc::new(Auth::new(config, Box::new(self.said.clone()))));
        Sources::new(&SourceConfig::default(), Arc::new(context)).unwrap()
    }

    /// The package's checksum read from the index, then the package fetched.
    fn fetch(&self, env: &[(&str, &str)]) -> anyhow::Result<()> {
        let sources = self.sources(env);
        let registry = sources.registry(source())?;
        assert_eq!(registry.checksum("secret", &self.package.version)?, self.package.checksum);
        let locked = Locked { source: source(), package: self.package.clone() };
        sources.fetch(&[locked])?;
        Ok(())
    }

    fn asked(&self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut *self.server.asked.lock().unwrap())
    }
}

fn source() -> SourceId {
    blaze_registry::registry_source(&format!("sparse+{BASE}")).unwrap()
}

const PROVIDERS: &str = "\n[registry]\nglobal-credential-providers = [\"cargo:token\"]\n";

#[test]
fn sends_the_token_cargo_sends() {
    let fixture =
        Fixture::new("token", "s3cret", PROVIDERS, "[registries.private]\ntoken = \"s3cret\"\n");
    fixture.fetch(&[]).unwrap();
    let token = Some("s3cret".to_owned());
    assert_eq!(
        fixture.asked(),
        [
            (format!("{BASE}config.json"), None),
            (format!("{BASE}config.json"), token.clone()),
            (format!("{BASE}se/cr/secret"), token.clone()),
            (format!("{DL}secret/1.0.0"), token.clone()),
        ]
    );
    assert!(fixture.home.join("registry/src").read_dir().unwrap().next().is_some());

    // The `config.json` kept says auth is required: the token goes with the first request.
    let sources = fixture.sources(&[]);
    let registry = sources.registry(source()).unwrap();
    registry.invalidate();
    registry.config().unwrap();
    assert_eq!(fixture.asked(), [(format!("{BASE}config.json"), token)]);
    assert!(fixture.said.0.lock().unwrap().is_empty());
}

#[test]
fn the_token_of_the_environment() {
    let fixture = Fixture::new("env", "from-env", PROVIDERS, "");
    fixture.fetch(&[("CARGO_REGISTRIES_PRIVATE_TOKEN", "from-env")]).unwrap();
    let asked = fixture.asked();
    assert_eq!(asked.last().unwrap().1.as_deref(), Some("from-env"));
}

#[test]
fn a_provider_has_to_be_configured() {
    let credentials = "[registries.private]\ntoken = \"s3cret\"\n";
    let fixture = Fixture::new("unconfigured", "s3cret", "", credentials);
    let err = fixture.fetch(&[]).unwrap_err();
    assert_eq!(
        format!("{:#}", err.root_cause()),
        "authenticated registries require a credential-provider to be configured\n\
         see https://doc.rust-lang.org/nightly/cargo/reference/registry-authentication.html for details"
    );
}

#[test]
fn says_what_cargo_says_of_no_token_or_a_refused_one() {
    let fixture = Fixture::new("missing", "s3cret", PROVIDERS, "");
    let err = fixture.fetch(&[]).unwrap_err();
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    assert!(
        chain.contains(
            &"no token found for `private`, please run `cargo login --registry private`\n\
              or use environment variable CARGO_REGISTRIES_PRIVATE_TOKEN"
                .to_owned()
        ),
        "{chain:#?}"
    );

    let credentials = "[registries.private]\ntoken = \"wrong\"\n";
    let fixture = Fixture::new("rejected", "s3cret", PROVIDERS, credentials);
    let err = fixture.fetch(&[]).unwrap_err();
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    let rejected = chain.iter().position(|it| {
        it == "token rejected for `private`, please run `cargo login --registry private`\n\
               or use environment variable CARGO_REGISTRIES_PRIVATE_TOKEN\n\
               note: the token does not include an authentication scheme"
    });
    let rejected = rejected.unwrap_or_else(|| panic!("{chain:#?}"));
    assert_eq!(
        chain[rejected + 1],
        format!(
            "failed to get successful HTTP response from `{BASE}config.json`, got 401\n\
             body:\nwho are you"
        )
    );
    assert_eq!(fixture.asked().len(), 2, "a 401 is not tried again");
}

#[test]
fn a_token_the_registry_ignores_is_warned_of() {
    let config = "\n[registry]\nglobal-credential-providers = [\"cargo:token-from-stdout x\"]\n";
    let credentials = "[registries.private]\ntoken = \"s3cret\"\n";
    let fixture = Fixture::new("ignored", "s3cret", config, credentials);
    let err = fixture.fetch(&[]).unwrap_err();
    let said = fixture.said.0.lock().unwrap().clone();
    assert_eq!(
        said,
        [format!(
            "warning: registry `private` has a token configured in {} that will be ignored \
             because the `cargo:token` credential provider is not listed in \
             `registry.global-credential-providers`",
            fixture.home.join("credentials.toml").display()
        )]
    );
    assert!(format!("{err:#}").contains("cargo:token-from-stdout x"), "{err:#}");
}
