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
    tmux_home::VERSION.to_string()
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
