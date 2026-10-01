mod common;
use std::time::Duration;
use tmux_home::{
    ipc::{Reply, Request, read_msg, write_msg},
    paths::Paths,
    tmux::source::SourceKind,
};
use tokio::{io::BufReader, net::UnixStream};

async fn connect(p: &Paths) -> UnixStream {
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(&p.sock).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket never appeared");
}

async fn ask(p: &Paths, req: Request) -> Option<Reply> {
    let s = connect(p).await;
    let (r, mut w) = s.into_split();
    write_msg(&mut w, &req).await.unwrap();
    read_msg(&mut BufReader::new(r)).await.unwrap()
}

fn v() -> String {
    tmux_home::BUILD_ID.to_string()
}

#[tokio::test]
async fn query_and_subscribe() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let Some(Reply::Snapshot { data, .. }) = ask(&p, Request::Query { v: v() }).await else {
        panic!()
    };
    assert_eq!(data.windows.len(), 1);

    let (r, mut w) = connect(&p).await.into_split();
    let mut r = BufReader::new(r);
    write_msg(
        &mut w,
        &Request::Subscribe {
            v: v(),
            client: "test".into(),
        },
    )
    .await
    .unwrap();
    let _first: Reply = read_msg(&mut r).await.unwrap().unwrap();
    s.tmux(&["new-window", "-d", "-t", "alpha", "-n", "pushed"]);
    let got = tokio::time::timeout(Duration::from_secs(2), read_msg::<_, Reply>(&mut r))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Reply::Snapshot { data, .. } = got else {
        panic!()
    };
    assert!(data.windows.iter().any(|w| w.name == "pushed"));

    s.tmux(&["kill-server"]);
    tokio::time::timeout(Duration::from_secs(5), d)
        .await
        .expect("daemon should exit with its server")
        .unwrap()
        .unwrap();
    assert!(!p.sock.exists());
}

/// A new subscriber's first snapshot is read fresh, not the source's last
/// one (a poll source's can be up to 500 ms old): the popup places its
/// cursor on the current window from it.
#[tokio::test]
async fn subscribe_starts_from_a_fresh_read() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    s.tmux(&["new-window", "-d", "-t", "alpha:", "-n", "two"]);
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let subscribe = || async {
        let (r, mut w) = connect(&p).await.into_split();
        write_msg(
            &mut w,
            &Request::Subscribe {
                v: v(),
                client: "test".into(),
            },
        )
        .await
        .unwrap();
        let mut r = BufReader::new(r);
        let Some(Reply::Snapshot { data, .. }) = read_msg(&mut r).await.unwrap() else {
            panic!()
        };
        (data, r, w)
    };
    let active = |data: &tmux_home::tmux::snapshot::Snapshot| {
        data.windows
            .iter()
            .find(|w| w.active)
            .map(|w| w.name.clone())
    };
    let (first, mut r, _w) = subscribe().await;
    assert!(first.windows.iter().any(|w| w.name == "two"));
    // ride the poll: right after it ticks, the next one is ~500 ms away
    s.tmux(&["rename-window", "-t", "=alpha:two", "tick"]);
    loop {
        let Some(Reply::Snapshot { data, .. }) =
            tokio::time::timeout(Duration::from_secs(2), read_msg(&mut r))
                .await
                .unwrap()
                .unwrap()
        else {
            panic!()
        };
        if data.windows.iter().any(|w| w.name == "tick") {
            break;
        }
    }
    s.tmux(&["select-window", "-t", "=alpha:tick"]);
    let (fresh, _r, _w) = subscribe().await;
    assert_eq!(active(&fresh).as_deref(), Some("tick"));

    s.tmux(&["kill-server"]);
    let _ = tokio::time::timeout(Duration::from_secs(5), d).await;
}

/// `refresh` re-reads tmux at once (no waiting for the poll), replies with
/// a new seq, and pushes the same snapshot to subscribers: nothing older
/// reaches them after it.
#[tokio::test]
async fn refresh_reads_now_and_pushes_in_order() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    s.wait_settled();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let (r, mut w) = connect(&p).await.into_split();
    let mut r = BufReader::new(r);
    write_msg(
        &mut w,
        &Request::Subscribe {
            v: v(),
            client: "test".into(),
        },
    )
    .await
    .unwrap();
    let Some(Reply::Snapshot {
        seq: first, epoch, ..
    }) = read_msg(&mut r).await.unwrap()
    else {
        panic!()
    };
    s.tmux(&["rename-window", "-t", "alpha:0", "right-now"]);
    let Some(Reply::Snapshot {
        seq,
        epoch: e2,
        data,
    }) = ask(&p, Request::Refresh { v: v() }).await
    else {
        panic!()
    };
    assert_eq!(e2, epoch, "same daemon");
    assert!(seq > first);
    assert!(data.windows.iter().any(|w| w.name == "right-now"));
    // the subscriber's pushes, in order, end with that seq
    loop {
        let Some(Reply::Snapshot { seq: got, data, .. }) =
            tokio::time::timeout(Duration::from_secs(2), read_msg(&mut r))
                .await
                .unwrap()
                .unwrap()
        else {
            panic!()
        };
        assert!(got <= seq, "pushed {got} past the refresh's {seq}");
        if got == seq {
            assert!(data.windows.iter().any(|w| w.name == "right-now"));
            break;
        }
    }
    // nothing changed since: a second refresh keeps the seq
    let Some(Reply::Snapshot { seq: again, .. }) = ask(&p, Request::Refresh { v: v() }).await
    else {
        panic!()
    };
    assert_eq!(again, seq);
    s.tmux(&["kill-server"]);
    let _ = tokio::time::timeout(Duration::from_secs(5), d).await;
}

#[tokio::test]
async fn second_daemon_is_a_no_op() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d1 = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let _ = connect(&p).await;
    let d2 = tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll);
    tokio::time::timeout(Duration::from_secs(2), d2)
        .await
        .expect("second daemon must return at once")
        .unwrap();
    assert!(
        ask(&p, Request::Query { v: v() }).await.is_some(),
        "first daemon still serving"
    );
    d1.abort();
}

#[tokio::test]
async fn stale_socket_file_is_replaced() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    std::fs::create_dir_all(p.sock.parent().unwrap()).unwrap();
    std::fs::write(&p.sock, b"stale").unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    assert!(ask(&p, Request::Query { v: v() }).await.is_some());
    d.abort();
}

#[tokio::test]
async fn version_mismatch_restarts() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll));
    let reply = ask(
        &p,
        Request::Query {
            v: "0.0.0-old".into(),
        },
    )
    .await;
    assert!(matches!(reply, Some(Reply::Restart)));
    tokio::time::timeout(Duration::from_secs(2), d)
        .await
        .expect("daemon exits after restart")
        .unwrap()
        .unwrap();
    assert!(!p.sock.exists());
}

/// A server with no sessions yet (TPM runs tmux-home.tmux from tmux.conf,
/// before the first session exists) must not make the daemon exit: it serves
/// an empty snapshot and pushes the first session when it appears.
#[tokio::test]
async fn serves_server_without_sessions() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start_empty();
    let p = Paths::for_socket(&s.socket).unwrap();
    let d = tokio::spawn(tmux_home::daemon::run(
        s.socket.clone(),
        SourceKind::Control,
    ));
    let (r, mut w) = connect(&p).await.into_split();
    let mut r = BufReader::new(r);
    write_msg(
        &mut w,
        &Request::Subscribe {
            v: v(),
            client: "test".into(),
        },
    )
    .await
    .unwrap();
    let Some(Reply::Snapshot { data, .. }) = read_msg(&mut r).await.unwrap() else {
        panic!("no initial snapshot")
    };
    assert!(data.sessions.is_empty());
    s.tmux(&["new-session", "-d", "-s", "first", "/bin/sh"]);
    let got = tokio::time::timeout(Duration::from_secs(2), read_msg::<_, Reply>(&mut r))
        .await
        .expect("first session not pushed")
        .unwrap()
        .unwrap();
    let Reply::Snapshot { data, .. } = got else {
        panic!()
    };
    assert!(data.sessions.iter().any(|x| x.name == "first"));
    s.tmux(&["kill-server"]);
    tokio::time::timeout(Duration::from_secs(5), d)
        .await
        .expect("daemon should exit with its server")
        .unwrap()
        .unwrap();
}

/// tmux kills `run-shell -b` jobs on kill-server; the daemon must still
/// clean up its socket (and release its lock) on SIGTERM, SIGHUP and SIGINT.
#[tokio::test]
async fn signals_clean_up_socket() {
    let _env = common::TestEnv::new();
    let s = common::TestServer::start();
    let p = Paths::for_socket(&s.socket).unwrap();
    for sig in ["TERM", "HUP", "INT"] {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tmux-home"))
            .args(["daemon", "--socket"])
            .arg(&s.socket)
            .spawn()
            .unwrap();
        let _ = connect(&p).await;
        let st = std::process::Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(child.id().to_string())
            .status()
            .unwrap();
        assert!(st.success());
        let mut exited = false;
        for _ in 0..100 {
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(exited, "daemon did not exit on SIG{sig}");
        assert!(!p.sock.exists(), "socket left behind after SIG{sig}");
    }
}

/// A socket path the platform can't bind (sun_path) fails early with a
/// clear error instead of an opaque bind failure.
#[tokio::test]
async fn overlong_socket_path_is_a_clear_error() {
    let env = common::TestEnv::new();
    let s = common::TestServer::start();
    let long = env.runtime.join("x".repeat(120));
    // SAFETY: single-threaded test process (RUST_TEST_THREADS=1).
    unsafe {
        std::env::set_var("TMUX_HOME_RUNTIME_DIR", &long);
    }
    let err = tmux_home::daemon::run(s.socket.clone(), SourceKind::Poll)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("too long"), "{err:#}");
    assert!(!long.exists(), "nothing created for an unusable path");
}
