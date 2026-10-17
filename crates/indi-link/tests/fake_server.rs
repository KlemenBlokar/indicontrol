//! End-to-end test of `Client` against a scripted fake indiserver.

use std::time::Duration;

use indi_link::{BlobMode, Client, Event, PropertyState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed")
}

/// Read from the socket until `needle` has been seen.
async fn read_until(sock: &mut tokio::net::TcpStream, seen: &mut String, needle: &str) {
    let mut buf = [0u8; 4096];
    while !seen.contains(needle) {
        let n = timeout(Duration::from_secs(5), sock.read(&mut buf))
            .await
            .expect("timed out reading from client")
            .unwrap();
        assert!(n > 0, "client closed connection; got so far: {seen}");
        seen.push_str(std::str::from_utf8(&buf[..n]).unwrap());
    }
}

#[tokio::test]
async fn round_trip_with_fake_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut seen = String::new();
        read_until(&mut sock, &mut seen, "<getProperties").await;
        sock.write_all(br#"
<defSwitchVector device="CCD Simulator" name="CONNECTION" label="Connection" group="Main Control" state="Idle" perm="rw" rule="OneOfMany" timeout="60">
  <defSwitch name="CONNECT" label="Connect">Off</defSwitch>
  <defSwitch name="DISCONNECT" label="Disconnect">On</defSwitch>
</defSwitchVector>
"#).await.unwrap();

        read_until(&mut sock, &mut seen, "</newSwitchVector>").await;
        sock.write_all(br#"<setSwitchVector device="CCD Simulator" name="CONNECTION" state="Ok" message="connected!">
  <oneSwitch name="CONNECT">On</oneSwitch><oneSwitch name="DISCONNECT">Off</oneSwitch>
</setSwitchVector>"#).await.unwrap();

        read_until(&mut sock, &mut seen, "</newNumberVector>").await;
        read_until(&mut sock, &mut seen, "</enableBLOB>").await;
        // Split a message across writes to exercise streaming.
        sock.write_all(b"<setBLOBVector device=\"CCD Simulator\" name=\"CCD1\" state=\"Ok\"><oneBLOB name=\"CCD1\" size=\"5\" format=\".fi").await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        sock.write_all(b"ts\">aGVsbG8=</oneBLOB></setBLOBVector>").await.unwrap();
        // Then hang up.
        drop(sock);
        seen
    });

    let (client, mut rx) = Client::connect(addr).await.unwrap();

    match next_event(&mut rx).await {
        Event::Defined { device, name } => {
            assert_eq!((device.as_str(), name.as_str()), ("CCD Simulator", "CONNECTION"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(!client.cache().devices["CCD Simulator"].is_connected());

    client.connect_device("CCD Simulator").unwrap();
    assert!(matches!(next_event(&mut rx).await, Event::Message { text, .. } if text == "connected!"));
    assert!(matches!(next_event(&mut rx).await, Event::Updated { .. }));
    {
        let cache = client.cache();
        let dev = &cache.devices["CCD Simulator"];
        assert!(dev.is_connected());
        assert_eq!(dev.properties["CONNECTION"].state, PropertyState::Ok);
    }

    client
        .send_number("CCD Simulator", "CCD_EXPOSURE", &[("CCD_EXPOSURE_VALUE", 2.5)])
        .unwrap();
    client.enable_blob("CCD Simulator", None, BlobMode::Also).unwrap();

    match next_event(&mut rx).await {
        Event::Blob { format, data, element, .. } => {
            assert_eq!(format, ".fits");
            assert_eq!(element, "CCD1");
            assert_eq!(data.as_slice(), b"hello");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(next_event(&mut rx).await, Event::Disconnected(None)));
    assert!(!client.is_connected());
    assert!(client.get_properties(None, None).is_err());

    let seen = server.await.unwrap();
    assert!(seen.contains(r#"<getProperties version="1.7"/>"#));
    assert!(seen.contains(r#"<oneSwitch name="CONNECT">On</oneSwitch>"#));
    assert!(seen.contains(r#"<oneNumber name="CCD_EXPOSURE_VALUE">2.5</oneNumber>"#));
    assert!(seen.contains(r#"<enableBLOB device="CCD Simulator">Also</enableBLOB>"#));
}
