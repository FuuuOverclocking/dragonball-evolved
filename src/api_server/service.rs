use api::helpers::fd::MergeFd;
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

    let result: ApiResult<Body> = async {
        let reply = waiter.await?;
        let mut body = Body::json(&reply.value)?;
        for fd in reply.fds {
            body.fds_mut().push(fd)?;
        }
        Ok(body)
    }
    .await;

    match result {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap_or_else(|e| {
                error!("failed to make response: {e}");
                Response::default()
            }),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::msg(format!("wait and make response: {e}")),
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

fn error_response(status: StatusCode, error: ApiError) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::json(&error).unwrap_or_else(|e| {
            error!("failed to serialize error to json: {e}, original error: {error}");
            Body::empty()
        }))
        .unwrap_or_else(|e| {
            error!("failed to make response: {e}, original status: {status}, error: {error}");
            Response::default()
        })
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
