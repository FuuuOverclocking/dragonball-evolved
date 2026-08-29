use api::metadata::ops;

fn main() {
    let ops = ops();
    println!("{:#}", ops[0].request.as_value());
    println!();
    println!();
    println!();
    println!("{:#}", ops[0].response.as_value());
}
