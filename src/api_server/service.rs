use api::helpers::fd::MergeFd;
use api::helpers::{SerializedReplyError, SerializedReplyWaiter};
use api::metadata::{OPERATIONS, Op};
use api::{ApiError, ApiResult, VmmCommand};
use hyper_uds::{Body, Request, Response, StatusCode, header};
use logger::error;
use vmm::VmmClient;

pub async fn service(vmm_client: VmmClient, req: Request<Body>) -> Response<Body> {
    let vmm_command = match parse(req) {
        Ok(cmd) => cmd,
        Err(resp) => return *resp,
    };

    let (request, waiter) = vmm_command.into_request();
    if vmm_client.send(request).is_err() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::msg("vmm already exited"),
        );
    }

    wait_and_respond(waiter).await
}

async fn wait_and_respond(waiter: SerializedReplyWaiter) -> Response<Body> {
    let reply = match waiter.await {
        Ok(reply) => reply,
        Err(SerializedReplyError::Operation(error)) => {
            return error_response(StatusCode::BAD_REQUEST, error);
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::msg(format!("wait and make response: {error}")),
            );
        }
    };

    let result = (|| -> ApiResult<Body> {
        let mut body = Body::json(&reply.value)?;
        for fd in reply.fds {
            body.fds_mut().push(fd)?;
        }
        Ok(body)
    })();

    match result {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::msg(format!("wait and make response: {error}")),
        ),
    }
}

fn parse(req: Request<Body>) -> Result<VmmCommand, Box<Response<Body>>> {
    let (parts, body) = req.into_parts();

    // Routes match path.
    let routes = OPERATIONS
        .iter()
        .filter_map(|op| op.route.map(|r| (op, r)))
        .filter(|(_, r)| r.path == parts.uri.path());

    // Routes match path and method.
    let Some((op, _)) = routes
        .clone()
        .find(|(_, route)| route.method == parts.method.as_str())
    else {
        return Err(Box::new(method_not_match(
            routes
                .map(|(_, route)| route.method)
                .collect::<Vec<_>>()
                .join(", "),
        )));
    };

    let cmd = match parse_body(op, body) {
        Ok(cmd) => cmd,
        Err(error) => return Err(Box::new(error_response(StatusCode::BAD_REQUEST, error))),
    };

    Ok(cmd)
}

fn method_not_match(allow_list: String) -> Response<Body> {
    if allow_list.is_empty() {
        return error_response(StatusCode::NOT_FOUND, ApiError::msg("route not found"));
    }

    let mut response = error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ApiError::msg("method not allowed"),
    );
    if let Ok(allow_list) = allow_list.parse() {
        response.headers_mut().insert(header::ALLOW, allow_list);
    }

    response
}

fn json_response(status: StatusCode, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

fn error_response(status: StatusCode, error: ApiError) -> Response<Body> {
    match Body::json(&error) {
        Ok(body) => json_response(status, body),
        Err(encoding_error) => {
            error!(
                "failed to serialize error to json: {encoding_error}, original status: {status}, error: {error}"
            );
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                Body::from(r#"{"error":"failed to serialize error response"}"#),
            )
        }
    }
}

fn parse_body(op: &Op, mut body: Body) -> ApiResult<VmmCommand> {
    let input = if body.is_empty() {
        b"null".as_slice()
    } else {
        body.as_bytes()
    };

    let mut command = (op.parse)(input.into())?;
    let mut fds: Vec<_> = (0..body.fds().len())
        .map(|index| body.fds_mut().take(index))
        .collect();
    command.merge_fd(&mut fds)?;

    if fds.iter().any(Option::is_some) {
        return Err(ApiError::msg("unexpected file descriptors"));
    }

    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::task::{Context, Poll, Waker};

    use api::VmmRequest;
    use api::helpers::SerializedReply;
    use api::live_upgrade::LiveUpgradeState;

    use super::*;

    fn ready(waiter: SerializedReplyWaiter) -> Response<Body> {
        let mut future = std::pin::pin!(wait_and_respond(waiter));
        let Poll::Ready(response) = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("response was not ready");
        };
        response
    }

    fn command(method: &str, path: &str, payload: &str) -> VmmCommand {
        parse(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::from(payload))
                .unwrap(),
        )
        .unwrap()
    }

    fn assert_error(response: Response<Body>, status: StatusCode, message: &str) {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            response.body().from_json::<serde_json::Value>().unwrap(),
            serde_json::json!({ "error": message })
        );
        assert!(response.body().fds().is_empty());
    }

    #[test]
    fn state_and_geometry_rejections_are_bad_requests_with_exact_diagnostics() {
        for (method, payload, message) in [
            ("PATCH", "0", "machine is not started"),
            ("PATCH", "0", "machine is not running"),
            (
                "PUT",
                r#"{"size_mib":256,"block_size_mib":3}"#,
                "hotplug block size 3145728 bytes is not a non-zero power of two",
            ),
        ] {
            let (request, waiter) =
                command(method, "/v1/machine/memory-hotplug", payload).into_request();
            let reply = match request {
                VmmRequest::ResizeMemory(_, reply) | VmmRequest::SetMemoryHotplug(_, reply) => {
                    reply
                }
                _ => panic!("expected a memory hotplug request"),
            };
            reply.send(Err(ApiError::msg(message)));
            assert_error(ready(waiter), StatusCode::BAD_REQUEST, message);
        }
    }

    #[test]
    fn disconnected_reply_is_an_internal_error() {
        let (request, waiter) = command("PATCH", "/v1/machine/memory-hotplug", "0").into_request();
        drop(request);
        assert_error(
            ready(waiter),
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("wait and make response: {}", oneshot::RecvError),
        );
    }

    #[test]
    fn json_encoding_failure_is_an_internal_error() {
        let error = serde_json::to_value(BTreeMap::from([(vec![0], 0)])).unwrap_err();
        let message = format!("wait and make response: {error}");
        let waiter: SerializedReplyWaiter = Box::pin(async move { Err(error.into()) });
        assert_error(ready(waiter), StatusCode::INTERNAL_SERVER_ERROR, &message);
    }

    #[test]
    fn descriptor_split_failure_is_an_internal_error() {
        let (request, waiter) =
            command("POST", "/v1/live-upgrade/handover-state", "{}").into_request();
        let VmmRequest::HandoverState(_, reply) = request else {
            panic!("expected HandoverState");
        };
        reply.send(Ok(LiveUpgradeState { test_fd: None }));
        assert_error(
            ready(waiter),
            StatusCode::INTERNAL_SERVER_ERROR,
            "wait and make response: test_fd is not bound",
        );
    }

    #[test]
    fn descriptor_delivery_failure_is_an_internal_error() {
        let reply = SerializedReply {
            value: serde_json::Value::Null,
            fds: (0..=hyper_uds::MAX_FDS)
                .map(|_| File::open("/dev/null").unwrap().into())
                .collect(),
        };
        let waiter: SerializedReplyWaiter = Box::pin(async move { Ok(reply) });
        assert_error(
            ready(waiter),
            StatusCode::INTERNAL_SERVER_ERROR,
            "wait and make response: too many file descriptors",
        );
    }

    #[test]
    fn invalid_payload_remains_a_bad_request() {
        let response = parse(
            Request::builder()
                .method("PATCH")
                .uri("/v1/machine/memory-hotplug")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
