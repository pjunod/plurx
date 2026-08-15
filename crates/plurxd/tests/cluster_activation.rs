//! Process-level proof that migration precedes every daemon side effect.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use plurx_core::auth::hash_password;
use plurx_core::store::{SqliteStore, UserStore};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind test port")
        .local_addr()
        .expect("test port")
        .port()
}

fn toml_string(value: &std::path::Path) -> String {
    value
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

struct Daemon(Child);

impl Daemon {
    fn stop(&mut self) {
        if self.0.try_wait().expect("daemon status").is_none() {
            #[cfg(unix)]
            {
                // SAFETY: the pid comes from the live child this guard owns.
                assert_eq!(
                    unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) },
                    0,
                    "signal daemon"
                );
            }
            #[cfg(not(unix))]
            self.0.kill().expect("stop daemon");
        }
        let status = self.0.wait().expect("wait daemon");
        #[cfg(unix)]
        assert!(status.success(), "daemon did not drain cleanly: {status}");
    }

    #[cfg(unix)]
    fn kill_ungracefully(&mut self) -> ExitStatus {
        if self.0.try_wait().expect("daemon status").is_none() {
            // SAFETY: the pid comes from the live child this guard owns.
            assert_eq!(
                unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGKILL) },
                0,
                "kill daemon without graceful shutdown"
            );
        }
        self.0.wait().expect("wait for killed daemon")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wait_for_bind(daemon: &mut Daemon, port: u16) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Some(status) = daemon.0.try_wait().expect("daemon status") {
            panic!("daemon exited before HTTP bind: {status}");
        }
        assert!(Instant::now() < deadline, "daemon HTTP bind timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn login_status(port: u16, username: &str, password: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/v1/auth/login"))
        .json(&serde_json::json!({
            "username": username,
            "password": password,
        }))
        .send()
        .await
        .expect("login request")
        .status()
}

#[test]
fn migration_quiescence_precedes_directory_cleanup_probes_and_http_bind() {
    let root = tempfile::tempdir().expect("activation process fixture");
    let data = root.path().join("data");
    std::fs::create_dir_all(data.join("sessions/live-session")).expect("session fixture");
    std::fs::write(
        data.join("sessions/live-session/owned-by-previous-boot"),
        b"preserve until migration completes",
    )
    .expect("session sentinel");
    drop(SqliteStore::open(&data.join("plurx.db")).expect("legacy SQLite source"));

    let config_path = root.path().join("plurx.toml");
    let server_port = free_port();
    std::fs::write(
        &config_path,
        format!(
            "[server]\n\
             bind = \"127.0.0.1:{server_port}\"\n\
             [storage]\n\
             data_dir = \"{}\"\n\
             [cluster]\n\
             raft_bind = \"127.0.0.1:{}\"\n\
             api_bind = \"127.0.0.1:{}\"\n\
             advertise_host = \"127.0.0.1\"\n",
            toml_string(&data),
            free_port(),
            free_port(),
        ),
    )
    .expect("activation config");

    let output = Command::new(env!("CARGO_BIN_EXE_plurxd"))
        .args([
            "--config",
            config_path.to_str().expect("config path"),
            "run",
        ])
        .env("PLURX_CLUSTER_ACTIVATION_FAILPOINT", "after-quiescence")
        .output()
        .expect("run injected daemon");
    assert_eq!(output.status.code(), Some(86));
    assert!(String::from_utf8_lossy(&output.stderr).contains("rollback command: plurxd run"));
    assert!(
        data.join("sessions/live-session/owned-by-previous-boot")
            .exists(),
        "create_dirs session cleanup must not run before migration completes"
    );

    let listener = TcpListener::bind(("127.0.0.1", server_port))
        .expect("HTTP port must not have been bound before migration completed");
    drop(listener);
}

/// The maintenance commands must reach TLS through the real binary.
///
/// `rustls` panics rather than erroring when a process reaches TLS with no
/// process-level provider, and `plurxd run` only ever got one as a side effect
/// of `hiqlite::start_node`, which these commands never call. So both aborted on
/// every activated node while the library-level contract test passed, because
/// that harness installs a provider of its own. Only the shipped binary can tell
/// the two apart, which is why this drives `CARGO_BIN_EXE_plurxd`.
///
/// Asserting on the *absence* of the panic rather than on success keeps the test
/// about the regression: reaching a reported error means TLS was negotiated.
#[test]
fn maintenance_commands_reach_tls_on_an_activated_node() {
    let root = tempfile::tempdir().expect("maintenance TLS fixture");
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).expect("data directory");
    drop(SqliteStore::open(&data.join("plurx.db")).expect("legacy SQLite source"));

    let config_path = root.path().join("plurx.toml");
    let server_port = free_port();
    std::fs::write(
        &config_path,
        format!(
            "[server]\n\
             bind = \"127.0.0.1:{server_port}\"\n\
             [storage]\n\
             data_dir = \"{}\"\n\
             [cluster]\n\
             raft_bind = \"127.0.0.1:{}\"\n\
             api_bind = \"127.0.0.1:{}\"\n\
             advertise_host = \"127.0.0.1\"\n",
            toml_string(&data),
            free_port(),
            free_port(),
        ),
    )
    .expect("maintenance config");

    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_plurxd"))
            .args([
                "--config",
                config_path.to_str().expect("config path"),
                "run",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start daemon"),
    );
    wait_for_bind(&mut daemon, server_port);
    assert!(data.join("hiqlite/activation.json").is_file());

    for args in [
        vec!["reset-password", "owner", "--password", "a-new-password"],
        vec!["refresh-metadata"],
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plurxd"));
        command.args(["--config", config_path.to_str().expect("config path")]);
        command.args(&args);
        let output = command.output().expect("run maintenance command");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("CryptoProvider"),
            "{args:?} aborted before TLS instead of using the activated voter: {stderr}"
        );
        assert!(
            !stderr.contains("panicked"),
            "{args:?} panicked against an activated node: {stderr}"
        );
        // The directory *is* activated, so nothing may tell the operator to
        // migrate it. `owner` does not exist, so a reported miss proves the
        // command read through the replicated store rather than failing earlier.
        assert!(
            !stderr.contains("is not activated"),
            "{args:?} refused an activated directory: {stderr}"
        );
    }

    daemon.stop();
}

#[test]
fn subsequent_plurxd_run_reopens_the_completed_replicated_target() {
    let root = tempfile::tempdir().expect("daemon activation fixture");
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).expect("data directory");
    drop(SqliteStore::open(&data.join("plurx.db")).expect("legacy SQLite source"));
    let source_before = std::fs::read(data.join("plurx.db")).expect("source bytes");

    let config_path = root.path().join("plurx.toml");
    let server_port = free_port();
    std::fs::write(
        &config_path,
        format!(
            "[server]\n\
             bind = \"127.0.0.1:{server_port}\"\n\
             [storage]\n\
             data_dir = \"{}\"\n\
             [cluster]\n\
             raft_bind = \"127.0.0.1:{}\"\n\
             api_bind = \"127.0.0.1:{}\"\n\
             advertise_host = \"127.0.0.1\"\n",
            toml_string(&data),
            free_port(),
            free_port(),
        ),
    )
    .expect("daemon config");

    let start = |activation_failpoint: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plurxd"));
        command
            .args([
                "--config",
                config_path.to_str().expect("config path"),
                "run",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(value) = activation_failpoint {
            command.env("PLURX_CLUSTER_ACTIVATION_FAILPOINT", value);
        }
        Daemon(command.spawn().expect("start daemon"))
    };

    let mut first = start(None);
    wait_for_bind(&mut first, server_port);
    assert!(data.join("hiqlite/activation.json").is_file());
    let contender = Command::new(env!("CARGO_BIN_EXE_plurxd"))
        .args([
            "--config",
            config_path.to_str().expect("config path"),
            "run",
        ])
        .output()
        .expect("run competing daemon");
    assert!(!contender.status.success());
    assert!(String::from_utf8_lossy(&contender.stderr)
        .contains("another plurxd process already owns the data directory"));
    first.stop();

    // The variable is meaningful only while activation is pending. A stale or
    // misspelled value in a service definition must not take an already-active
    // server offline when no injection boundary can be reached.
    let mut second = start(Some("not-a-real-failpoint"));
    wait_for_bind(&mut second, server_port);
    assert!(data.join("hiqlite/activation.json").is_file());
    second.stop();

    assert_eq!(
        std::fs::read(data.join("plurx.db")).expect("retained source"),
        source_before,
        "subsequent replicated boots must not reopen or migrate retained SQLite"
    );
}

/// A one-voter node must recover current acknowledged state after SIGKILL.
///
/// Hiqlite treats a stale state-machine lock as proof that the SQLite state
/// machine may be inconsistent with its Raft log. The `auto-heal` path removes
/// and rebuilds that state machine from the retained local log/snapshot; merely
/// unlinking the lock would skip the reconstruction. The password change is
/// acknowledged after activation, so reading it after restart proves the
/// rebuild preserved state newer than the retained pre-activation SQLite file.
#[cfg(unix)]
#[tokio::test]
async fn activated_one_voter_rebuilds_current_state_after_sigkill() {
    let root = tempfile::tempdir().expect("SIGKILL recovery fixture");
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).expect("data directory");
    let source = SqliteStore::open(&data.join("plurx.db")).expect("legacy SQLite source");
    source
        .create_user(
            "recovery-owner",
            &hash_password("password-before-kill").expect("initial password hash"),
            true,
        )
        .await
        .expect("seed recovery user");
    drop(source);

    let config_path = root.path().join("plurx.toml");
    let server_port = free_port();
    std::fs::write(
        &config_path,
        format!(
            "[server]\n\
             bind = \"127.0.0.1:{server_port}\"\n\
             [storage]\n\
             data_dir = \"{}\"\n\
             [cluster]\n\
             raft_bind = \"127.0.0.1:{}\"\n\
             api_bind = \"127.0.0.1:{}\"\n\
             advertise_host = \"127.0.0.1\"\n",
            toml_string(&data),
            free_port(),
            free_port(),
        ),
    )
    .expect("SIGKILL recovery config");

    let start = || {
        Daemon(
            Command::new(env!("CARGO_BIN_EXE_plurxd"))
                .args([
                    "--config",
                    config_path.to_str().expect("config path"),
                    "run",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start recovery daemon"),
        )
    };

    let mut first = start();
    wait_for_bind(&mut first, server_port);
    assert_eq!(
        login_status(server_port, "recovery-owner", "password-before-kill").await,
        reqwest::StatusCode::OK
    );

    let update = Command::new(env!("CARGO_BIN_EXE_plurxd"))
        .args([
            "--config",
            config_path.to_str().expect("config path"),
            "reset-password",
            "recovery-owner",
            "--password",
            "password-after-kill",
        ])
        .output()
        .expect("acknowledged post-activation password update");
    assert!(
        update.status.success(),
        "password update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(
        login_status(server_port, "recovery-owner", "password-after-kill").await,
        reqwest::StatusCode::OK
    );

    let status = first.kill_ungracefully();
    assert!(!status.success(), "SIGKILL must be ungraceful: {status}");
    assert!(
        data.join("hiqlite/state_machine/lock").is_file(),
        "the regression requires Hiqlite's stale-lock recovery path"
    );

    let mut recovered = start();
    wait_for_bind(&mut recovered, server_port);
    assert_eq!(
        login_status(server_port, "recovery-owner", "password-after-kill").await,
        reqwest::StatusCode::OK,
        "the acknowledged post-activation update must survive log replay"
    );
    assert_eq!(
        login_status(server_port, "recovery-owner", "password-before-kill").await,
        reqwest::StatusCode::UNAUTHORIZED,
        "recovery must not fall back to the retained pre-activation source"
    );
    recovered.stop();
}
