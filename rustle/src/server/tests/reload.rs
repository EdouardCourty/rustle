use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---- reload ----

fn reload_toml(key: &str, extra: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nadmin_listen = \"\"\ncontrol_socket = \"\"\n\
         [auth]\napi_keys = [\"{key}\"]\n\
         [filter]\nblock_private = false\n\
         [log]\naccess = \"off\"\n{extra}"
    )
}

async fn get_with_key(addr: SocketAddr, up: SocketAddr, key: &str) -> String {
    use base64::Engine as _;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("u:{key}"));
    let req = format!(
        "GET http://{up}/a HTTP/1.1\r\nHost: {up}\r\nProxy-Authorization: Basic {cred}\r\nConnection: close\r\n\r\n"
    );
    status(&send_recv(addr, req.as_bytes()).await)
}

#[tokio::test]
async fn reload_swaps_keys_and_keeps_the_old_config_on_error() {
    let dir = std::env::temp_dir().join(format!("rustle-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, reload_toml("old", "")).unwrap();
    let up = origin().await;
    let cfg = Config::load(&path).unwrap();
    let srv = start_with(Arc::new(cfg), Some(path.clone())).unwrap();
    let addr = srv.local_addr();
    assert!(get_with_key(addr, up, "old").await.contains("200"));

    // New key, changed limit, and a restart-only setting.
    std::fs::write(&path, reload_toml("new", "[limits]\nmax_connections = 5\n")).unwrap();
    let r = srv.reload().unwrap();
    assert!(r.restart_required.is_empty(), "{r}");
    assert!(get_with_key(addr, up, "old").await.contains("407"));
    assert!(get_with_key(addr, up, "new").await.contains("200"));

    std::fs::write(
        &path,
        reload_toml("new", "").replace("admin_listen = \"\"", "admin_listen = \"127.0.0.1:0\""),
    )
    .unwrap();
    let r = srv.reload().unwrap();
    assert_eq!(r.restart_required, ["server.admin_listen"]);

    // The control socket is bound at startup: a new path is reported too.
    std::fs::write(
        &path,
        reload_toml("new", "").replace("control_socket = \"\"", "control_socket = \"auto\""),
    )
    .unwrap();
    let r = srv.reload().unwrap();
    assert_eq!(r.restart_required, ["server.control_socket"]);

    // A broken file is refused whole: the running config stays.
    std::fs::write(
        &path,
        reload_toml("broken", "[filter]\ndeny = [\"10.0.0.0/99\"]\n"),
    )
    .unwrap();
    assert!(srv.reload().is_err());
    std::fs::write(&path, "[server]\nlisen = 1\n").unwrap();
    assert!(srv.reload().is_err());
    assert!(get_with_key(addr, up, "new").await.contains("200"));
    assert!(get_with_key(addr, up, "broken").await.contains("407"));

    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn control_socket_reloads_and_rejects_unknown_commands() {
    use tokio::net::UnixStream;
    let dir = std::env::temp_dir().join(format!("rustle-ctl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (cfg_path, sock) = (dir.join("config.toml"), dir.join("ctl.sock"));
    let text = reload_toml("k1", "").replace(
        "control_socket = \"\"",
        &format!("control_socket = {:?}", sock.to_string_lossy()),
    );
    std::fs::write(&cfg_path, &text).unwrap();
    let srv = start_with(
        Arc::new(Config::load(&cfg_path).unwrap()),
        Some(cfg_path.clone()),
    )
    .unwrap();

    async fn ask(sock: &std::path::Path, cmd: &str) -> String {
        let mut c = UnixStream::connect(sock).await.unwrap();
        c.write_all(cmd.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        out
    }
    assert_eq!(ask(&sock, "reload\n").await, "ok reloaded\n");
    assert!(ask(&sock, "bogus\n")
        .await
        .starts_with("error unknown command"));
    std::fs::write(&cfg_path, "[server]\nlisen = 1\n").unwrap();
    assert!(ask(&sock, "reload\n")
        .await
        .starts_with("error reload failed"));
    // 0600: owner only.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o600
    );

    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
    assert!(!sock.exists(), "socket file left behind");
}

#[tokio::test]
async fn reload_without_a_config_file_is_an_error() {
    let srv = start(test_config(1, 10)).unwrap();
    assert!(srv.reload().is_err());
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
