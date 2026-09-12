use super::*;
use crate::rendezvous::tests::ExchangeDirectory;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    sync::Barrier,
    thread,
};

#[test]
fn identity_survives_reload_and_concurrent_initialization() {
    let directory = ExchangeDirectory::new();
    let path = directory.0.join("device");
    let barrier = Arc::new(Barrier::new(8));
    let workers = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                IrohDeviceIdentity::load_or_create(path)
                    .expect("identity")
                    .public_id()
            })
        })
        .collect::<Vec<_>>();
    let ids = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .collect::<Vec<_>>();
    assert!(ids.iter().all(|id| id == &ids[0]));
    let key = path.join(KEY_FILE);
    let before = fs::metadata(&key).expect("key metadata");
    assert_eq!(before.len(), 32);
    assert_eq!(before.mode() & 0o777, 0o600);
    let identity = IrohDeviceIdentity::load_or_create(&path).expect("reload");
    assert_eq!(identity.public_id(), ids[0]);
    assert_eq!(fs::metadata(&key).expect("same key").ino(), before.ino());
    assert_eq!(fs::read_dir(&path).expect("private entries").count(), 1);
    assert!(format!("{identity:?}").contains(identity.public_id().as_str()));
    let other = IrohDeviceIdentity::load_or_create(directory.0.join("other")).expect("other");
    assert_ne!(identity.public_id(), other.public_id());
}

#[test]
fn malformed_key_lengths_fail_without_overwrite() {
    let directory = ExchangeDirectory::new();
    for length in [0, 31, 33, 4097] {
        let path = directory.0.join(length.to_string());
        let storage = PrivateDirectory::open_or_create(&path).expect("storage");
        let fixture = vec![0u8; length];
        assert!(
            storage
                .create_new(OsStr::new(KEY_FILE), &fixture)
                .expect("fixture")
        );
        assert!(IrohDeviceIdentity::load_or_create(&path).is_err());
        assert_eq!(fs::read(path.join(KEY_FILE)).expect("unchanged"), fixture);
    }
}

#[test]
fn private_key_rejects_symlinks_special_files_and_insecure_modes() {
    let directory = ExchangeDirectory::new();
    let path = directory.0.join("device");
    IrohDeviceIdentity::load_or_create(&path).expect("identity");
    let key = path.join(KEY_FILE);
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).expect("insecure key");
    assert!(IrohDeviceIdentity::load_or_create(&path).is_err());
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("restore");
    fs::rename(&key, path.join("original")).expect("move fixture");
    symlink("original", &key).expect("symlink key");
    assert!(IrohDeviceIdentity::load_or_create(&path).is_err());
    fs::remove_file(&key).expect("remove test symlink");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &key,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .expect("FIFO");
    assert!(IrohDeviceIdentity::load_or_create(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("insecure directory");
    assert!(IrohDeviceIdentity::load_or_create(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("restore directory");
    let alias = directory.0.join("alias");
    symlink(&path, &alias).expect("directory symlink");
    assert!(IrohDeviceIdentity::load_or_create(alias).is_err());
    assert!(IrohDeviceIdentity::load_or_create(directory.0.join("absent/child")).is_err());
}

fn peer() -> IrohPeerIdentity {
    IrohPeerIdentity(SecretKey::generate().public().to_string())
}

#[test]
fn profiles_round_trip_and_never_replace_an_existing_profile() {
    let directory = ExchangeDirectory::new();
    let profile = IrohConnectionProfile::new(
        peer(),
        IrohNetwork::N0,
        vec![
            "127.0.0.1:1234".parse().expect("address"),
            "[::1]:4321".parse().expect("IPv6"),
        ],
    )
    .expect("profile");
    assert_eq!(
        profile
            .to_string()
            .parse::<IrohConnectionProfile>()
            .expect("round trip"),
        profile
    );
    let path = directory.0.join("source.profile");
    profile.save_new(&path).expect("save");
    assert_eq!(IrohConnectionProfile::load(&path).expect("load"), profile);
    assert!(profile.save_new(&path).is_err());
    assert_eq!(
        IrohConnectionProfile::load(&path).expect("still same"),
        profile
    );
}

#[test]
fn profile_and_allowlist_validation_is_bounded_and_fail_closed() {
    let id = peer();
    let valid = format!("peer={}\nnetwork=n0\n", id.as_str());
    assert!(valid.parse::<IrohConnectionProfile>().is_ok());
    for invalid in [
        valid.replace("n0", "direct"),
        valid.replace("n0", "unknown"),
        format!("{valid}network=n0\n"),
        format!("{valid}peer={}\n", id.as_str()),
        format!("{valid}secret=ignored\n"),
        format!("{valid}address=not-a-socket\n"),
        format!("{valid}{}", "address=127.0.0.1:1\n".repeat(33)),
        "x".repeat(4097),
        "network=n0\n".into(),
        format!("peer={}\n", id.as_str()),
    ] {
        assert!(invalid.parse::<IrohConnectionProfile>().is_err());
    }
    assert!(IrohTrustedPeers::new(Vec::new()).is_err());
    assert!(IrohTrustedPeers::new(vec![id.clone(); 33]).is_err());
    let trusted = IrohTrustedPeers::new(vec![id.clone(); 2]).expect("duplicates normalized");
    assert_eq!(trusted.0.len(), 1);
    assert!(trusted.contains(&id.as_str().parse().expect("id")));
    assert!(!trusted.contains(&SecretKey::generate().public()));
}
