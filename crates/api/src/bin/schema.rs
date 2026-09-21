use api::metadata::METADATA;

fn main() {
    let ops = &*METADATA;
    println!("{:#}", ops[0].request.as_value());
    println!();
    println!();
    println!();
    println!("{:#}", ops[0].response.as_value());
}
