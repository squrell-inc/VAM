use std::any::TypeId;

use serde::{de::DeserializeOwned, Serialize};

use crate::{Error, Result};

const HTTP_VERSION: &'static str = "HTTP/1.0";

/// Trait for encoding a struct into an HTTP request.
pub trait RequestTrait {
    /// The type of the payload to be serialized.
    type Payload: Serialize + 'static;

    /// Encodes the payload into an HTTP request.
    fn encode(&self) -> Result<Vec<u8>> {
        // method uri version
        // "GET /version HTTP/1.0\r\n\r\n";
        let mut request = format!("{} {} {}\r\n", self.method(), self.path(), HTTP_VERSION);

        let request = if TypeId::of::<Self::Payload>() == TypeId::of::<Empty>() {
            request.push_str("\r\n");
            request.as_bytes().to_vec()
        } else {
            let payload = self.payload();
            let mut payload = serde_json::to_vec(&payload)
                .map_err(|e| Error::Event(format!("serde_json encode: {e}")))?;
            // add `Content-Length` header
            request.push_str(&format!("Content-Length: {}\r\n", payload.len()));
            // empty line splitting headers and body
            request.push_str("\r\n");
            // add body
            let mut request = request.as_bytes().to_vec();
            request.append(&mut payload);
            request
        };

        Ok(request)
    }

    /// Returns the HTTP method (e.g., "PATCH", "GET").
    fn method(&self) -> &'static str;

    /// Returns the endpoint path (e.g., "/balloon").
    fn path(&self) -> String;

    /// Returns the payload to be serialized.
    fn payload(&self) -> &Self::Payload;
}

/// Trait for decoding an HTTP response into a struct.
pub trait ResponseTrait {
    /// The type of the payload to be deserialized.
    type Payload: DeserializeOwned + 'static;

    /// Get the HTTP response status code
    fn status_code(response: &Vec<u8>) -> Result<u16> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut res = httparse::Response::new(&mut headers);
        let body_start = res
            .parse(&response)
            .map_err(|e| Error::Event(format!("HTTP response parse error: {e}")))?;
        if body_start.is_partial() {
            return Err(Error::Event("Incomplete response".into()));
        }
        res.code
            .ok_or_else(|| Error::Event("Bad HTTP response".into()))
    }

    /// Decodes the HTTP response into a payload.
    fn decode(response: &Vec<u8>) -> Result<Self::Payload> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut res = httparse::Response::new(&mut headers);

        let body_start = res
            .parse(&response)
            .map_err(|e| Error::Event(format!("HTTP response parse error: {e}")))?;
        if body_start.is_partial() {
            return Err(Error::Event("Incomplete response".into()));
        }
        let body_start = body_start.unwrap(); // safe because is_partial() is false

        // A 4xx/5xx is an error, never a payload: Firecracker answers a refused request
        // with `{"fault_message": ...}`, which an empty-payload event would otherwise
        // swallow (a failed drive PATCH or snapshot load must not look like success).
        if let Some(code) = res.code {
            if !(200..300).contains(&code) {
                let fault = std::str::from_utf8(&response[body_start..])
                    .ok()
                    .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                    .and_then(|v| {
                        v.get("fault_message")
                            .and_then(|m| m.as_str().map(String::from))
                    })
                    .unwrap_or_default();
                return Err(Error::Event(format!("HTTP {code}: {fault}")));
            }
        }

        let content_length = res
            .headers
            .iter()
            .find(|h| h.name.to_lowercase() == "content-length")
            .and_then(|h| {
                let s = std::str::from_utf8(h.value).ok()?;
                s.parse::<usize>().ok()
            });

        match content_length {
            Some(content_length) => {
                let body = &response[body_start..(body_start + content_length)];
                if TypeId::of::<Self::Payload>() == TypeId::of::<Empty>() {
                    // Firecracker 1.16+ often returns `{}` with Content-Length for empty PUT bodies.
                    if body.is_empty() || body == b"{}" || body == b"null" {
                        let payload: Self::Payload = serde_json::from_str("null")
                            .map_err(|e| Error::Event(format!("serde_json decode: {e}")))?;
                        return Ok(payload);
                    }
                }
                let payload: Self::Payload = serde_json::from_slice(body)
                    .map_err(|e| Error::Event(format!("serde_json decode: {e}")))?;
                Ok(payload)
            }
            None if TypeId::of::<Self::Payload>() == TypeId::of::<Empty>() => {
                // just no payload, fine
                // FIXME: ugly to use "null", could there be prettier solution?
                let payload: Self::Payload = serde_json::from_str("null")
                    .map_err(|e| Error::Event(format!("serde_json decode: {e}")))?;
                Ok(payload)
            }
            _ => Err(Error::Event("Bad HTTP response".into())),
        }
    }
}

pub trait EventTrait: RequestTrait + ResponseTrait {}

macro_rules! impl_event_traits {
    // Other conditions
    ($struct_name:ident, $method:expr, $path:expr, $req_payload:ty, $res_payload:ty) => {
        pub struct $struct_name<'a>(pub &'a $req_payload);

        impl<'a> RequestTrait for $struct_name<'a> {
            type Payload = $req_payload;

            fn method(&self) -> &'static str {
                $method
            }

            fn path(&self) -> String {
                $path.into()
            }

            fn payload(&self) -> &Self::Payload {
                &self.0
            }
        }

        impl<'a> ResponseTrait for $struct_name<'a> {
            type Payload = $res_payload;
        }

        impl<'a> EventTrait for $struct_name<'a> {}

        paste::paste! {
            pub struct [<$struct_name Owned>](
                pub $req_payload
                // field!($req_payload)
            );

            impl RequestTrait for [<$struct_name Owned>] {
                type Payload = $req_payload;

                fn method(&self) -> &'static str {
                    $method
                }

                fn path(&self) -> String {
                    $path.into()
                }

                fn payload(&self) -> &Self::Payload {
                    &self.0
                }
            }

            impl ResponseTrait for [<$struct_name Owned>] {
                type Payload = $res_payload;
            }

            impl EventTrait for [<$struct_name Owned>] {}
        }
    };

    ($struct_name:ident, $method:expr, $path:expr, $id:ident, $req_payload:ty, $res_payload:ty) => {
        pub struct $struct_name<'a>(pub &'a $req_payload);

        impl<'a> RequestTrait for $struct_name<'a> {
            type Payload = $req_payload;

            fn method(&self) -> &'static str {
                $method
            }

            fn path(&self) -> String {
                format!("{}/{}", $path, &self.0.$id)
            }

            fn payload(&self) -> &Self::Payload {
                &self.0
            }
        }

        impl<'a> ResponseTrait for $struct_name<'a> {
            type Payload = $res_payload;
        }

        impl<'a> EventTrait for $struct_name<'a> {}

        paste::paste! {
            pub struct [<$struct_name Owned>](pub $req_payload);

            impl RequestTrait for [<$struct_name Owned>] {
                type Payload = $req_payload;

                fn method(&self) -> &'static str {
                    $method
                }

                fn path(&self) -> String {
                    format!("{}/{}", $path, &self.0.$id)
                }

                fn payload(&self) -> &Self::Payload {
                    &self.0
                }
            }

            impl ResponseTrait for [<$struct_name Owned>] {
                type Payload = $res_payload;
            }

            impl EventTrait for [<$struct_name Owned>] {}
        }
    };
}

use crate::models::*;
const GET: &'static str = "GET";
const PUT: &'static str = "PUT";
const PATCH: &'static str = "PATCH";

impl_event_traits!(DescribeInstance, GET, "/", Empty, InstanceInfo);
impl_event_traits!(CreateSyncAction, PUT, "/actions", InstanceActionInfo, Empty);
impl_event_traits!(DescribeBalloonConfig, GET, "/balloon", Empty, Balloon);
impl_event_traits!(PutBalloon, PUT, "/balloon", Balloon, Empty);
impl_event_traits!(PatchBalloon, PATCH, "/balloon", BalloonUpdate, Empty);
impl_event_traits!(
    DescribeBalloonStats,
    GET,
    "/balloon/statistics",
    Empty,
    BalloonStats
);
impl_event_traits!(
    PatchBalloonStatsInterval,
    PATCH,
    "/balloon/statistics",
    BalloonStatsUpdate,
    Empty
);
impl_event_traits!(PutGuestBootSource, PUT, "/boot-source", BootSource, Empty);
impl_event_traits!(PutCpuConfiguration, PUT, "/cpu-config", CPUConfig, Empty);
impl_event_traits!(PutGuestDriveByID, PUT, "/drives", drive_id, Drive, Empty);
impl_event_traits!(
    PatchGuestDriveByID,
    PATCH,
    "/drives",
    drive_id,
    PartialDrive,
    Empty
);
impl_event_traits!(PutLogger, PUT, "/logger", Logger, Empty);
impl_event_traits!(
    GetMachineConfiguration,
    GET,
    "/machine-config",
    Empty,
    MachineConfiguration
);
impl_event_traits!(
    PutMachineConfiguration,
    PUT,
    "/machine-config",
    MachineConfiguration,
    Empty
);
impl_event_traits!(
    PatchMachineConfiguration,
    PATCH,
    "/machine-config",
    MachineConfiguration,
    Empty
);
impl_event_traits!(PutMetrics, PUT, "/metrics", Metrics, Empty);
impl_event_traits!(PutMmds, PUT, "/mmds", MmdsContentsObject, Empty);
impl_event_traits!(PatchMmds, PATCH, "/mmds", MmdsContentsObject, Empty);
impl_event_traits!(GetMmds, GET, "/mmds", Empty, MmdsContentsObject);
impl_event_traits!(PutMmdsConfig, PUT, "/mmds/config", MmdsConfig, Empty);
impl_event_traits!(PutEntropyDevice, PUT, "/entropy", EntropyDevice, Empty);
impl_event_traits!(
    PutGuestNetworkInterfaceByID,
    PUT,
    "/network-interfaces",
    iface_id,
    NetworkInterface,
    Empty
);
impl_event_traits!(
    PatchGuestNetworkInterfaceByID,
    PATCH,
    "/network-interfaces",
    iface_id,
    PartialNetworkInterface,
    Empty
);
impl_event_traits!(
    CreateSnapshot,
    PUT,
    "/snapshot/create",
    SnapshotCreateParams,
    Empty
);
impl_event_traits!(
    LoadSnapshot,
    PUT,
    "/snapshot/load",
    SnapshotLoadParams,
    Empty
);
impl_event_traits!(
    GetFirecrackerVersion,
    GET,
    "/version",
    Empty,
    FirecrackerVersion
);
impl_event_traits!(PatchVm, PATCH, "/vm", Vm, Empty);
impl_event_traits!(
    GetExportVmConfig,
    GET,
    "/vm/config",
    Empty,
    FullVmConfiguration
);
impl_event_traits!(PutGuestVsock, PUT, "/vsock", Vsock, Empty);
impl_event_traits!(PatchGuestVsock, PATCH, "/vsock", PartialVsock, Empty);

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::models::Empty;

    fn resp(code: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn a_refused_request_is_an_error_with_firecrackers_reason() {
        let bad = resp(
            "400 Bad Request",
            r#"{"fault_message":"Load snapshot error: no such file"}"#,
        );
        let e = LoadSnapshotOwned::decode(&bad).unwrap_err().to_string();
        assert!(e.contains("400") && e.contains("no such file"), "{e}");
        let e = PatchGuestDriveByIDOwned::decode(&resp("500 Internal Server Error", ""))
            .unwrap_err()
            .to_string();
        assert!(e.contains("500"), "{e}");
    }

    #[test]
    fn success_responses_still_decode() {
        let _: Empty = LoadSnapshotOwned::decode(&resp("204 No Content", "")).unwrap();
        let _: Empty = LoadSnapshotOwned::decode(&resp("200 OK", "{}")).unwrap();
        // HTTP/1.0 style, no Content-Length.
        let _: Empty = LoadSnapshotOwned::decode(
            &b"HTTP/1.1 204 \r\nServer: Firecracker API\r\n\r\n".to_vec(),
        )
        .unwrap();
    }
}
