#![cfg(unix)]

use std::sync::Arc;
use tempfile::tempdir;
use tokio::net::UnixStream;

use vam_storage::{Drive9Client, Drive9Server, FilesystemCheckpointCoordinator};

#[tokio::test]
async fn test_drive9_loopback_operations() {
    let sandbox_dir = tempdir().unwrap();
    let manifests_dir = tempdir().unwrap();

    let checkpoint_coordinator =
        Arc::new(FilesystemCheckpointCoordinator::new(manifests_dir.path()));
    let server = Arc::new(Drive9Server::new(
        sandbox_dir.path().to_path_buf(),
        10 * 1024 * 1024, // 10 MiB quota
        checkpoint_coordinator,
    ));

    let (client_stream, server_stream) = UnixStream::pair().unwrap();

    // Spawn server connection handler
    let server_clone = server.clone();
    tokio::spawn(async move {
        server_clone.handle_connection(server_stream).await;
    });

    // Create client
    let client = Drive9Client::new(client_stream);

    // 1. Create a directory
    client.mkdir("test_dir", 0o755).await.unwrap();

    // 2. Open a new file
    let fd = client
        .open("test_dir/hello.txt", 64 | 2, 0o644)
        .await
        .unwrap(); // O_CREAT | O_RDWR

    // 3. Write data
    let test_data = b"Hello from local Drive9 FS frames over vsock!".to_vec();
    let written = client.write(fd, 0, test_data.clone()).await.unwrap();
    assert_eq!(written as usize, test_data.len());

    // 4. Fsync & Close
    client.fsync(fd, false).await.unwrap();
    client.close(fd).await.unwrap();

    // 5. Open for reading
    let fd_read = client.open("test_dir/hello.txt", 0, 0).await.unwrap(); // O_RDONLY

    // 6. Read back
    let read_data = client.read(fd_read, 0, 100).await.unwrap();
    assert_eq!(read_data, test_data);
    client.close(fd_read).await.unwrap();

    // 7. Stat file
    let stat = client.stat("test_dir/hello.txt").await.unwrap();
    assert_eq!(stat.size, test_data.len() as u64);
    assert!(!stat.is_dir);

    // 8. Readdir
    let entries = client.readdir("test_dir").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello.txt");
    assert_eq!(
        entries[0].stat.as_ref().unwrap().size,
        test_data.len() as u64
    );

    // 9. Checkpoint
    let checkpoint_ref = client
        .checkpoint("urn:vam:workspace:local.vam.dev/acme/demo", "initial")
        .await
        .unwrap();
    assert_eq!(
        checkpoint_ref.artifact_id.unwrap().urn,
        "urn:vam:workspace:local.vam.dev/acme/demo"
    );
    assert!(checkpoint_ref.digest.is_some());
}

#[tokio::test]
async fn test_drive9_loopback_concurrent_multiplexing() {
    let sandbox_dir = tempdir().unwrap();
    let manifests_dir = tempdir().unwrap();

    let checkpoint_coordinator =
        Arc::new(FilesystemCheckpointCoordinator::new(manifests_dir.path()));
    let server = Arc::new(Drive9Server::new(
        sandbox_dir.path().to_path_buf(),
        10 * 1024 * 1024,
        checkpoint_coordinator,
    ));

    let (client_stream, server_stream) = UnixStream::pair().unwrap();

    let server_clone = server.clone();
    tokio::spawn(async move {
        server_clone.handle_connection(server_stream).await;
    });

    let client = Arc::new(Drive9Client::new(client_stream));

    // Spawn 10 concurrent tasks reading and writing to different files
    let mut handles = Vec::new();
    for i in 0..10 {
        let client_clone = client.clone();
        handles.push(tokio::spawn(async move {
            let filename = format!("file_{}.txt", i);
            let fd = client_clone.open(&filename, 64 | 2, 0o644).await.unwrap();

            let data = format!("payload_{}", i).into_bytes();
            client_clone.write(fd, 0, data.clone()).await.unwrap();
            client_clone.close(fd).await.unwrap();

            let fd_read = client_clone.open(&filename, 0, 0).await.unwrap();
            let read_bytes = client_clone.read(fd_read, 0, 100).await.unwrap();
            assert_eq!(read_bytes, data);
            client_clone.close(fd_read).await.unwrap();
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn test_drive9_sandbox_enforcement() {
    let sandbox_dir = tempdir().unwrap();
    let manifests_dir = tempdir().unwrap();

    let checkpoint_coordinator =
        Arc::new(FilesystemCheckpointCoordinator::new(manifests_dir.path()));
    let server = Arc::new(Drive9Server::new(
        sandbox_dir.path().to_path_buf(),
        10 * 1024 * 1024,
        checkpoint_coordinator,
    ));

    let (client_stream, server_stream) = UnixStream::pair().unwrap();

    let server_clone = server.clone();
    tokio::spawn(async move {
        server_clone.handle_connection(server_stream).await;
    });

    let client = Drive9Client::new(client_stream);

    // Try to open a file outside sandbox
    let res = client.open("../secret.txt", 0, 0).await;
    assert!(res.is_err());
}
