//! The repository connector (docs/CONTROL_PLANE.md section 13): fixture
//! monorepos under `tests/fixtures/repos/` crawled into `builtin:code`,
//! incremental crawls, CODEOWNERS, the walk's limits and a clone of a local
//! bare repository through the test-only `file://` switch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use onto_intelli::connectors::repo;
use onto_intelli::core::Dir;
use onto_intelli::{Config, Intelli, Principal};

/// A directory of its own, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}{}", &onto_intelli::core::time::new_id("x")[2..]));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/repos").join(name)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}

/// A copy of a fixture in a temporary directory (tests change it).
fn checkout(name: &str) -> TempDir {
    let t = TempDir::new(&format!("onto-repo-{name}-"));
    copy_tree(&fixture(name), &t.0);
    t
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn admin() -> Principal {
    Principal::admin("ada")
}

async fn engine() -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    Intelli::open(cfg).await.unwrap()
}

async fn connect(i: &Intelli, name: &str, url: &str, repo: Value) -> Value {
    let spec = json!({ "name": name, "kind": "repo", "url": url, "every_secs": 0, "repo": repo });
    i.call(&admin(), "connector.add", spec).await.unwrap();
    crawl(i, name).await
}

async fn crawl(i: &Intelli, name: &str) -> Value {
    let r = i.call(&admin(), "connector.crawl", json!({ "name": name })).await.unwrap();
    r["crawls"][0].clone()
}

fn entity(i: &Intelli, id: &str) -> Option<Value> {
    i.ontology().read(|g, _| g.get(id).map(|e| serde_json::to_value(e).unwrap()))
}

fn ids_of(i: &Intelli, kind: &str) -> Vec<String> {
    let mut v: Vec<String> =
        i.ontology().read(|g, _| g.entities().filter(|e| e.kind == kind).map(|e| e.id.clone()).collect::<Vec<_>>());
    v.sort();
    v
}

/// `depends_on` targets of a package with their `kind`.
fn deps(i: &Intelli, id: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = i.ontology().read(|g, _| {
        g.neighbors(id, "depends_on", Dir::Out)
            .into_iter()
            .map(|to| {
                let kind = g.get_link(id, "depends_on", to).unwrap().props["kind"].as_str().unwrap().to_string();
                (to.to_string(), kind)
            })
            .collect()
    });
    v.sort();
    v
}

fn owners(i: &Intelli, id: &str) -> Vec<String> {
    let mut v: Vec<String> =
        i.ontology().read(|g, _| g.neighbors(id, "owns", Dir::In).into_iter().map(str::to_string).collect());
    v.sort();
    v
}

fn pair(to: &str, kind: &str) -> (String, String) {
    (to.to_string(), kind.to_string())
}

#[tokio::test]
async fn cargo_workspace_packages_dependencies_and_codeowners() {
    let i = engine().await;
    let t = checkout("cargo");
    let url = t.0.to_string_lossy().to_string();
    let r = connect(&i, "mono", &url, json!({})).await;
    assert_eq!(r["repo"]["packages"], 4, "{r:#}");
    assert_eq!(r["repo"]["ecosystems"]["cargo"], 4);
    assert_eq!(r["repo"]["codeowners"], ".github/CODEOWNERS");
    assert_eq!(
        ids_of(&i, "package"),
        [
            "package:mono.cargo.acme-api",
            "package:mono.cargo.acme-core",
            "package:mono.cargo.acme-gen",
            "package:mono.cargo.acme-testkit"
        ]
    );
    // Excluded by [workspace] exclude.
    assert!(entity(&i, "package:mono.cargo.acme-skipped").is_none());

    let core = entity(&i, "package:mono.cargo.acme-core").unwrap();
    assert_eq!(core["title"], "acme-core");
    assert_eq!(core["workspace"], "acme");
    assert_eq!(core["props"]["version"], "1.4.0", "version.workspace = true is resolved");
    assert_eq!(core["props"]["ecosystem"], "cargo");
    assert_eq!(core["props"]["path"], "crates/core");
    assert_eq!(core["props"]["manifest"], "crates/core/Cargo.toml");
    assert_eq!(core["props"]["language"], "rust");
    assert_eq!(core["provenance"]["source"], "connector:repo");
    assert_eq!(core["provenance"]["connector"], "mono");
    assert_eq!(core["provenance"]["external_ref"], url.as_str());
    let api = entity(&i, "package:mono.cargo.acme-api").unwrap();
    assert_eq!(api["props"]["description"], "Shared description");
    assert_eq!(entity(&i, "package:mono.cargo.acme-testkit").unwrap()["props"]["private"], true);

    assert_eq!(
        deps(&i, "package:mono.cargo.acme-api"),
        [
            pair("package:mono.cargo.acme-core", "normal"),
            pair("package:mono.cargo.acme-gen", "build"),
            pair("package:mono.cargo.acme-testkit", "dev"),
        ]
    );
    assert_eq!(deps(&i, "package:mono.cargo.acme-testkit"), [pair("package:mono.cargo.acme-core", "normal")]);
    let in_repo: Vec<String> = i.ontology().read(|g, _| {
        g.neighbors("repository:mono", "in_repository", Dir::In).into_iter().map(str::to_string).collect()
    });
    assert_eq!(in_repo.len(), 4);
    let repo_e = entity(&i, "repository:mono").unwrap();
    assert_eq!(repo_e["props"]["url"], url.as_str());

    // CODEOWNERS: the last matching rule wins; `*` owns the repository.
    assert_eq!(owners(&i, "repository:mono"), ["team:acme-platform"]);
    assert_eq!(owners(&i, "package:mono.cargo.acme-core"), ["team:acme-backend"]);
    assert_eq!(owners(&i, "package:mono.cargo.acme-api"), ["person:ada", "team:acme-api-team"]);
    assert_eq!(owners(&i, "package:mono.cargo.acme-testkit"), ["team:acme-qa"]);
    assert_eq!(owners(&i, "package:mono.cargo.acme-gen"), ["person:gh-grace"]);
    let team = entity(&i, "team:acme-api-team").unwrap();
    assert_eq!(team["title"], "@acme/api-team");
    assert_eq!(r["repo"]["owners"], 6);
}

#[tokio::test]
async fn npm_pnpm_go_and_uv_workspaces() {
    let i = engine().await;
    let npm = checkout("npm");
    connect(&i, "web", &npm.0.to_string_lossy(), json!({})).await;
    assert_eq!(
        ids_of(&i, "package"),
        ["package:web.npm.acme.ui", "package:web.npm.acme.utils", "package:web.npm.acme.web"]
    );
    let web = entity(&i, "package:web.npm.acme.web").unwrap();
    assert_eq!(web["title"], "@acme/web");
    assert_eq!(web["props"]["private"], true);
    assert_eq!(web["props"]["language"], "typescript");
    assert_eq!(entity(&i, "package:web.npm.acme.ui").unwrap()["props"]["language"], "javascript");
    // Normal wins over optional for the same target; external packages are not mapped.
    assert_eq!(
        deps(&i, "package:web.npm.acme.web"),
        [pair("package:web.npm.acme.ui", "normal"), pair("package:web.npm.acme.utils", "dev")]
    );
    assert_eq!(deps(&i, "package:web.npm.acme.ui"), [pair("package:web.npm.acme.utils", "normal")]);
    assert_eq!(owners(&i, "package:web.npm.acme.web"), ["team:acme-web-team"]);
    assert_eq!(owners(&i, "package:web.npm.acme.ui"), ["team:acme-frontend"]);
    assert_eq!(owners(&i, "repository:web"), ["team:acme-frontend"]);

    let pnpm = checkout("pnpm");
    connect(&i, "libs", &pnpm.0.to_string_lossy(), json!({})).await;
    let libs: Vec<String> = ids_of(&i, "package").into_iter().filter(|p| p.starts_with("package:libs.")).collect();
    assert_eq!(libs, ["package:libs.npm.lib-a", "package:libs.npm.lib-b"], "!libs/legacy is excluded");
    assert_eq!(deps(&i, "package:libs.npm.lib-b"), [pair("package:libs.npm.lib-a", "peer")]);

    let go = checkout("go");
    connect(&i, "gomono", &go.0.to_string_lossy(), json!({})).await;
    let mods: Vec<String> = ids_of(&i, "package").into_iter().filter(|p| p.starts_with("package:gomono.")).collect();
    assert_eq!(mods, ["package:gomono.go.example.com.mono.api", "package:gomono.go.example.com.mono.lib"]);
    let api = entity(&i, "package:gomono.go.example.com.mono.api").unwrap();
    assert_eq!(api["title"], "api");
    assert_eq!(api["props"]["module"], "example.com/mono/api");
    assert_eq!(api["aliases"], json!(["example.com/mono/api"]));
    assert_eq!(
        deps(&i, "package:gomono.go.example.com.mono.api"),
        [pair("package:gomono.go.example.com.mono.lib", "normal")]
    );

    let uv = checkout("uv");
    connect(&i, "py", &uv.0.to_string_lossy(), json!({})).await;
    let py: Vec<String> = ids_of(&i, "package").into_iter().filter(|p| p.starts_with("package:py.")).collect();
    assert_eq!(
        py,
        [
            "package:py.python.acme-cli",
            "package:py.python.acme-core",
            "package:py.python.acme-docs",
            "package:py.python.acme-platform",
            "package:py.python.acme-testing",
        ],
        "packages/scratch is excluded"
    );
    let core = entity(&i, "package:py.python.acme-core").unwrap();
    assert_eq!(core["title"], "acme-core", "PEP 503 names");
    assert_eq!(core["props"]["private"], true);
    assert_eq!(entity(&i, "package:py.python.acme-testing").unwrap()["props"]["description"], "Test helpers (Poetry).");
    assert_eq!(
        deps(&i, "package:py.python.acme-cli"),
        [
            pair("package:py.python.acme-core", "normal"),
            pair("package:py.python.acme-docs", "optional"),
            pair("package:py.python.acme-testing", "dev"),
        ]
    );
    assert_eq!(deps(&i, "package:py.python.acme-platform"), [pair("package:py.python.acme-core", "normal")]);
    assert_eq!(
        deps(&i, "package:py.python.acme-testing"),
        [pair("package:py.python.acme-core", "normal"), pair("package:py.python.acme-docs", "dev")]
    );
}

#[tokio::test]
async fn crawls_are_incremental() {
    let i = engine().await;
    let t = checkout("cargo");
    let first = connect(&i, "mono", &t.0.to_string_lossy(), json!({})).await;
    assert!(first["upserted"].as_u64().unwrap() >= 5, "{first:#}");
    let rev = i.ontology().revision();
    let gen_updated = entity(&i, "package:mono.cargo.acme-gen").unwrap()["updated_at"].clone();
    let core_history =
        i.call(&admin(), "entity.history", json!({ "id": "package:mono.cargo.acme-core" })).await.unwrap();

    // Nothing changed: nothing is written.
    let second = crawl(&i, "mono").await;
    assert_eq!(second["upserted"], 0, "{second:#}");
    assert_eq!(second["deleted"], 0);
    assert!(second["notes"].to_string().contains("unchanged"), "{second:#}");
    assert_eq!(i.ontology().revision(), rev, "an unchanged crawl writes nothing");
    let again = i.call(&admin(), "entity.history", json!({ "id": "package:mono.cargo.acme-core" })).await.unwrap();
    assert_eq!(again, core_history);

    // A package disappears, ownership moves: only that is written.
    std::fs::remove_dir_all(t.0.join("crates/testkit")).unwrap();
    let owners_file = t.0.join(".github/CODEOWNERS");
    let text = std::fs::read_to_string(&owners_file)
        .unwrap()
        .replace("/crates/              @acme/backend", "/crates/ @acme/core-team");
    std::fs::write(&owners_file, text).unwrap();
    let third = crawl(&i, "mono").await;
    assert!(entity(&i, "package:mono.cargo.acme-testkit").is_none(), "{third:#}");
    assert!(
        entity(&i, "team:acme-qa").is_none(),
        "an owner the connector created and that owns nothing now is removed"
    );
    assert_eq!(
        deps(&i, "package:mono.cargo.acme-api"),
        [pair("package:mono.cargo.acme-core", "normal"), pair("package:mono.cargo.acme-gen", "build")],
        "links of a deleted package go with it"
    );
    assert_eq!(owners(&i, "package:mono.cargo.acme-core"), ["team:acme-core-team"]);
    assert!(entity(&i, "team:acme-backend").is_none());
    assert!(third["deleted"].as_u64().unwrap() >= 2, "{third:#}");
    assert!(third["unchanged"].as_u64().unwrap() >= 3, "{third:#}");
    assert_eq!(
        entity(&i, "package:mono.cargo.acme-gen").unwrap()["updated_at"],
        gen_updated,
        "untouched packages are not rewritten"
    );

    // Changed options are a change too (include narrows to crates/).
    i.call(
        &admin(),
        "connector.add",
        json!({ "name": "mono", "kind": "repo", "url": t.0.to_string_lossy(), "every_secs": 0, "repo": { "include": ["/crates/"] } }),
    )
    .await
    .unwrap();
    let fourth = crawl(&i, "mono").await;
    assert!(entity(&i, "package:mono.cargo.acme-gen").is_none(), "{fourth:#}");
    assert!(entity(&i, "person:gh-grace").is_none());
    assert!(entity(&i, "package:mono.cargo.acme-core").is_some());

    // Purging the connector deletes what it wrote.
    let removed = i.call(&admin(), "connector.remove", json!({ "name": "mono", "purge": true })).await.unwrap();
    assert!(removed["deleted"].as_u64().unwrap() >= 3, "{removed:#}");
    assert!(ids_of(&i, "package").is_empty());
    assert!(entity(&i, "repository:mono").is_none());
}

#[tokio::test]
async fn existing_owners_and_the_owner_map() {
    let i = engine().await;
    i.call(&admin(), "entity.upsert", json!({ "id": "team:payments", "kind": "team", "title": "Payments Team" }))
        .await
        .unwrap();
    i.call(
        &admin(),
        "entity.upsert",
        json!({ "id": "team:acme-backend", "kind": "team", "title": "Backend (curated)" }),
    )
    .await
    .unwrap();
    let t = checkout("cargo");
    connect(&i, "mono", &t.0.to_string_lossy(), json!({ "owners": { "@acme/api-team": "team:payments" } })).await;
    assert_eq!(owners(&i, "package:mono.cargo.acme-api"), ["person:ada", "team:payments"]);
    assert_eq!(
        entity(&i, "team:payments").unwrap()["title"],
        "Payments Team",
        "an existing owner is linked, not rewritten"
    );
    assert_eq!(entity(&i, "team:acme-backend").unwrap()["title"], "Backend (curated)");
    assert_eq!(owners(&i, "package:mono.cargo.acme-core"), ["team:acme-backend"]);
    assert!(entity(&i, "team:acme-api-team").is_none());

    // A curated owner stays when CODEOWNERS stops naming it; only its link goes.
    let owners_file = t.0.join(".github/CODEOWNERS");
    let text = std::fs::read_to_string(&owners_file).unwrap().replace("@acme/backend", "@acme/core-team");
    std::fs::write(&owners_file, text).unwrap();
    crawl(&i, "mono").await;
    assert_eq!(owners(&i, "package:mono.cargo.acme-core"), ["team:acme-core-team"]);
    assert!(entity(&i, "team:acme-backend").is_some());

    // Bad owner maps are refused when the connector is registered.
    let bad = json!({ "name": "x", "kind": "repo", "url": t.0.to_string_lossy(), "repo": { "owners": { "@a/b": "service:x" } } });
    assert!(i.call(&admin(), "connector.add", bad).await.is_err());
}

#[tokio::test]
async fn the_walk_skips_and_limits() {
    let i = engine().await;
    let t = checkout("npm");
    let pkg = |name: &str| format!("{{\"name\": \"{name}\", \"version\": \"1.0.0\"}}");
    // Never read, though `packages/*` would make them members: dependency
    // and build directories, hidden ones, excluded globs.
    for dir in ["node_modules", "target", "dist", "build", "vendor", ".venv", ".hidden"] {
        write(&t.0.join("packages").join(dir).join("package.json"), &pkg(&format!("@acme/ignored-{dir}")));
    }
    write(&t.0.join("packages/ui/node_modules/x/package.json"), &pkg("@acme/nested"));
    write(&t.0.join("packages/experimental/package.json"), &pkg("@acme/experimental"));
    // Deeper than 12 levels below the root: not read.
    let deep: PathBuf = (0..13).map(|n| format!("d{n}")).collect();
    write(&t.0.join("packages").join(&deep).join("package.json"), &pkg("@acme/deep"));
    // A manifest over 1 MB is skipped with a note.
    write(
        &t.0.join("packages/huge/package.json"),
        &format!("{{\"name\": \"@acme/huge\", \"x\": \"{}\"}}", "a".repeat(1_100_000)),
    );
    #[cfg(unix)]
    {
        let outside = TempDir::new("onto-repo-outside-");
        write(&outside.0.join("package.json"), &pkg("@acme/outside"));
        std::os::unix::fs::symlink(&outside.0, t.0.join("packages/linked")).unwrap();
        let r = connect(&i, "web", &t.0.to_string_lossy(), json!({ "exclude": ["packages/experimental"] })).await;
        assert!(r["notes"].to_string().contains("larger than 1 MB"), "{r:#}");
        assert!(r["notes"].to_string().contains("deeper than 12"), "{r:#}");
    }
    #[cfg(not(unix))]
    connect(&i, "web", &t.0.to_string_lossy(), json!({ "exclude": ["packages/experimental"] })).await;
    assert_eq!(
        ids_of(&i, "package"),
        ["package:web.npm.acme.ui", "package:web.npm.acme.utils", "package:web.npm.acme.web"]
    );

    // include keeps only the packages it covers.
    let u = checkout("npm");
    connect(&i, "apps", &u.0.to_string_lossy(), json!({ "include": ["apps/*"] })).await;
    let apps: Vec<String> = ids_of(&i, "package").into_iter().filter(|p| p.starts_with("package:apps.")).collect();
    assert_eq!(apps, ["package:apps.npm.acme.web"]);
    // Its dependencies outside the included set are not mapped.
    assert!(deps(&i, "package:apps.npm.acme.web").is_empty());
}

#[tokio::test]
async fn refusals() {
    // Without the code layer there is no package type.
    let mut cfg = Config::in_memory();
    cfg.ontology.extend = vec!["builtin:data-catalog".into()];
    let i = Intelli::open(cfg).await.unwrap();
    let t = checkout("go");
    let spec = json!({ "name": "g", "kind": "repo", "url": t.0.to_string_lossy(), "every_secs": 0 });
    i.call(&admin(), "connector.add", spec).await.unwrap();
    let e = i.call(&admin(), "connector.crawl", json!({ "name": "g" })).await.unwrap_err();
    assert!(e.to_string().contains("builtin:code"), "{e}");

    let i = engine().await;
    for url in [
        "ext::sh -c id",
        "--upload-pack=touch /tmp/x",
        "http://github.com/acme/platform",
        "https://token@github.com/acme/platform",
        "relative/path",
        "git@host:-oProxyCommand=x",
    ] {
        let spec = json!({ "name": "bad", "kind": "repo", "url": url });
        assert!(i.call(&admin(), "connector.add", spec).await.is_err(), "{url}");
    }
    let spec =
        json!({ "name": "b", "kind": "repo", "url": "https://github.com/acme/x", "repo": { "branch": "--orphan" } });
    assert!(i.call(&admin(), "connector.add", spec).await.is_err());
    let missing = json!({ "name": "m", "kind": "repo", "url": "/nonexistent/onto-repo", "every_secs": 0 });
    i.call(&admin(), "connector.add", missing).await.unwrap();
    assert!(i.call(&admin(), "connector.crawl", json!({ "name": "m" })).await.is_err());
    // Connectors need an admin.
    let viewer = Principal::new("v", vec![onto_intelli::Role::Viewer], vec!["*".into()]);
    let spec = json!({ "name": "v", "kind": "repo", "url": t.0.to_string_lossy() });
    assert!(i.call(&viewer, "connector.add", spec).await.is_err());
}

fn git_ok() -> bool {
    std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=onto test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(["-c", "init.defaultBranch=main", "-c", "protocol.file.allow=always"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clones_a_local_bare_repository() {
    if !git_ok() {
        eprintln!("skipped clones_a_local_bare_repository: no git binary");
        return;
    }
    let base = TempDir::new("onto-repo-git-");
    let work = base.0.join("work");
    copy_tree(&fixture("go"), &work);
    git(&work, &["init", "-q"]);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "first"]);
    let bare = base.0.join("platform.git");
    git(&base.0, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);

    repo::testing_allow_file_urls(true);
    repo::testing_cache_dir(Some(base.0.join("cache")));
    let url = format!("file://{}", bare.display());
    let i = engine().await;
    let r = connect(&i, "platform", &url, json!({ "branch": "main" })).await;
    assert_eq!(r["repo"]["packages"], 2, "{r:#}");
    let commit = r["repo"]["commit"].as_str().unwrap().to_string();
    assert_eq!(commit.len(), 40);
    assert_eq!(r["repo"]["branch"], "main");
    let api = entity(&i, "package:platform.go.example.com.mono.api").unwrap();
    assert_eq!(api["provenance"]["external_ref"], format!("{url}@{commit}"));
    assert_eq!(entity(&i, "repository:platform").unwrap()["props"]["default_branch"], "main");
    let clone = repo::clone_dir(&url, Some("main"));
    assert!(clone.starts_with(base.0.join("cache")) && clone.join(".git").is_dir());

    // Same commit: nothing to do.
    let rev = i.ontology().revision();
    let again = crawl(&i, "platform").await;
    assert_eq!(again["upserted"], 0, "{again:#}");
    assert_eq!(i.ontology().revision(), rev);

    // A new commit upstream is fetched and reset to.
    write(&work.join("lib/go.mod"), "module example.com/mono/lib\n\ngo 1.22\n\nrequire example.com/mono/api v0.0.0\n");
    git(&work, &["commit", "-q", "-am", "lib needs api"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let third = crawl(&i, "platform").await;
    assert_ne!(third["repo"]["commit"], commit, "{third:#}");
    assert_eq!(
        deps(&i, "package:platform.go.example.com.mono.lib"),
        [pair("package:platform.go.example.com.mono.api", "normal")]
    );

    // A repository that does not exist fails the crawl with git's message.
    let missing = format!("file://{}", base.0.join("missing.git").display());
    let spec = json!({ "name": "missing", "kind": "repo", "url": missing, "every_secs": 0 });
    i.call(&admin(), "connector.add", spec).await.unwrap();
    let e = i.call(&admin(), "connector.crawl", json!({ "name": "missing" })).await.unwrap_err();
    assert!(e.to_string().contains("git clone failed"), "{e}");
    repo::testing_cache_dir(None);
}
