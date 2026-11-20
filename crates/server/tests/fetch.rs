//! `LOAD CSV` from web URLs through the server's HTTP client, against a local stub server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use graphbuff::{Database, ResultValue};

/// Serves a few paths on a local port; returns its `http://127.0.0.1:port`.
fn stub() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut line = String::new();
            let mut r = BufReader::new(conn.try_clone().unwrap());
            r.read_line(&mut line).unwrap();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
            }
            let path = line.split(' ').nth(1).unwrap_or("").to_string();
            let reply: Vec<u8> = match path.as_str() {
                "/data/plain.csv" => b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nname\nAnn\nBob\n".to_vec(),
                "/data/chunked.csv" => {
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nname\n\r\n4\r\nCat\n\r\n0\r\n\r\n".to_vec()
                }
                "/data/people.csv.gz" => {
                    let gz = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../engine/tests/data/people.csv.gz")).unwrap();
                    let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", gz.len()).into_bytes();
                    out.extend(gz);
                    out
                }
                "/data/moved" => b"HTTP/1.1 301 Moved\r\nLocation: plain.csv\r\nContent-Length: 0\r\n\r\n".to_vec(),
                "/data/away" => b"HTTP/1.1 302 Found\r\nLocation: /secret.csv\r\nContent-Length: 0\r\n\r\n".to_vec(),
                "/data/short.csv" => b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nname\n".to_vec(),
                _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            };
            let _ = conn.write_all(&reply);
        }
    });
    base
}

fn names(db: &Database, url: &str) -> Result<Vec<String>, String> {
    let q = format!("LOAD CSV WITH HEADERS FROM '{url}' AS row RETURN row.name AS name");
    let r = db.execute(&q).map_err(|e| e.to_string())?;
    Ok(r.rows
        .iter()
        .map(|row| match &row[0] {
            ResultValue::String(s) => s.clone(),
            o => panic!("{o:?}"),
        })
        .collect())
}

#[test]
fn load_csv_over_http() {
    let base = stub();
    let db = Database::in_memory();
    let prefixes = graphbuff_server::fetch::parse_prefixes(&format!("{base}/data/")).unwrap();
    db.set_csv_urls(Some(graphbuff_server::fetch::csv_urls(prefixes)));

    assert_eq!(names(&db, &format!("{base}/data/plain.csv")).unwrap(), ["Ann", "Bob"]);
    assert_eq!(names(&db, &format!("{base}/data/chunked.csv")).unwrap(), ["Cat"]);
    assert_eq!(names(&db, &format!("{base}/data/moved")).unwrap(), ["Ann", "Bob"]);
    assert_eq!(names(&db, &format!("{base}/data/people.csv.gz")).unwrap().len(), 5000);
    let e = names(&db, &format!("{base}/data/away")).unwrap_err();
    assert!(e.contains("none of the allowed URL prefixes"), "{e}");
    let e = names(&db, &format!("{base}/secret.csv")).unwrap_err();
    assert!(e.contains("allowed URL prefixes"), "{e}");
    let e = names(&db, &format!("{base}/data/missing.csv")).unwrap_err();
    assert!(e.contains("404"), "{e}");
    let e = names(&db, &format!("{base}/data/short.csv")).unwrap_err();
    assert!(e.contains("ended after"), "{e}");

    // The opener on its own.
    let mut body = String::new();
    graphbuff_server::fetch::open(&format!("{base}/data/plain.csv"), &[format!("{base}/")])
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "name\nAnn\nBob\n");
}
