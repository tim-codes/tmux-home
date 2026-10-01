use tmux_home::store::{CLOSED_MAX, ClosedWindow, Store};

fn entry(n: u32) -> ClosedWindow {
    ClosedWindow {
        session: "s".into(),
        index: n,
        prev: "@1".into(),
        next: "-".into(),
        automatic_rename: n.is_multiple_of(2),
        active: 1,
        layout: "abcd,80x24,0,0,1".into(),
        name: format!("w{n}"),
        paths: vec!["/tmp".into(), "/a b".into()],
    }
}

#[test]
fn round_trip_lifo_capped() {
    let dir = std::env::temp_dir().join(format!("th-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(dir.join("srv"), None);
    assert_eq!(store.pop().unwrap(), None);
    for n in 0..12 {
        store.push(entry(n)).unwrap();
    }
    assert_eq!(store.len().unwrap(), CLOSED_MAX);
    // a fresh Store reads the same file
    let again = Store::new(dir.join("srv"), None);
    assert_eq!(again.pop().unwrap(), Some(entry(11)));
    assert_eq!(again.pop().unwrap(), Some(entry(10)));
    assert_eq!(again.len().unwrap(), 8);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn imports_bash_stack_once() {
    let dir = std::env::temp_dir().join(format!("th-store-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let legacy = dir.join("closed");
    let us = '\x1f';
    std::fs::write(
        &legacy,
        format!("alpha{us}5{us}@1{us}-{us}off{us}1{us}lay{us}old name{us}/usr{us}/etc\n"),
    )
    .unwrap();
    let store = Store::new(dir.join("srv"), Some(legacy.clone()));
    let e = store.pop().unwrap().unwrap();
    assert_eq!(
        (
            e.session.as_str(),
            e.index,
            e.name.as_str(),
            e.automatic_rename
        ),
        ("alpha", 5, "old name", false)
    );
    assert_eq!(e.paths, ["/usr", "/etc"]);
    assert!(!legacy.exists());
    assert!(dir.join("closed.imported").exists());
    assert_eq!(store.pop().unwrap(), None);
    std::fs::remove_dir_all(&dir).unwrap();
}
