use arbora::{
    merkle::{blob_object, hash_object},
    store::{HttpStore, ObjectStore},
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
};

#[test]
#[ignore = "requires loopback sockets, which are unavailable in some sandboxes"]
fn http_store_uses_head_and_get_and_is_read_only() {
    let object = Arc::new(blob_object(b"from HTTP"));
    let hash = hash_object(&object);
    let key = format!("/prefix/objects/{}/{}", &hash[7..9], &hash[9..]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let served = Arc::clone(&object);
    let server = thread::spawn(move || {
        for expected_method in ["HEAD", "GET"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with(&format!("{expected_method} {key} HTTP/1.1")));
            let body = if expected_method == "GET" {
                served.as_slice()
            } else {
                &[]
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });

    let store = HttpStore::new(format!("http://{address}/"), "/prefix/").unwrap();
    assert!(store.exists(&hash).unwrap());
    assert_eq!(store.get(&hash).unwrap(), object.as_slice());
    assert!(store.put(&hash, &object).is_err());
    server.join().unwrap();
}
