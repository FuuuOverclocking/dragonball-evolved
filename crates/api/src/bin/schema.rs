use api::metadata::METADATA;

fn main() -> api::ApiResult<()> {
    match std::env::args().nth(1).as_deref() {
        Some("config") => println!("{:#}", api::config::schema().as_value()),
        None => {
            let ops = &*METADATA;
            println!("{:#}", ops[0].request.as_value());
            println!();
            println!();
            println!();
            println!("{:#}", ops[0].response.as_value());
        }
        Some(_) => return Err(api::ApiError::msg("usage: schema [config]")),
    }
    Ok(())
}
