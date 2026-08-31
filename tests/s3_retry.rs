use arbora::store::{ObjectStore, S3Options, S3Store};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

fn server(statuses: Vec<u16>) -> (String, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = 0;
        for status in statuses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("HEAD /bucket/objects/ab/"));
            requests += 1;
            if status == 200 {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            } else {
                let body =
                    b"<Error><Code>ServiceUnavailable</Code><Message>retry</Message></Error>";
                write!(
                    stream,
                    "HTTP/1.1 {status} Service Unavailable\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        }
        requests
    });
    (endpoint, handle)
}

fn store(endpoint: String, max_attempts: u32) -> S3Store {
    S3Store::new(S3Options {
        bucket: "bucket".into(),
        endpoint: Some(endpoint),
        region: Some("auto".into()),
        access_key_id: Some("test-access-key".into()),
        secret_access_key: Some("test-secret-key".into()),
        force_path_style: true,
        retry_max_attempts: max_attempts,
        retry_max_backoff_ms: 10,
        ..S3Options::default()
    })
    .unwrap()
}

#[test]
#[ignore = "requires loopback sockets, which are unavailable in some sandboxes"]
fn retries_r2_503_until_success() {
    let (endpoint, server) = server(vec![503, 503, 200]);
    let store = store(endpoint, 4);
    let hash = format!("blake3:{}", "ab".repeat(32));
    assert!(store.exists(&hash).unwrap());
    assert_eq!(server.join().unwrap(), 3);
}

#[test]
#[ignore = "requires loopback sockets, which are unavailable in some sandboxes"]
fn stops_at_the_configured_attempt_bound() {
    let (endpoint, server) = server(vec![503, 503, 503]);
    let store = store(endpoint, 3);
    let hash = format!("blake3:{}", "ab".repeat(32));
    assert!(store.exists(&hash).is_err());
    assert_eq!(server.join().unwrap(), 3);
}
