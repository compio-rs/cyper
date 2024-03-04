use cyper::Client;

#[compio::main]
async fn main() {
    let client = Client::new();
    let response = client
        .get("https://www.example.com/")
        .unwrap()
        .send()
        .await
        .unwrap();
    println!("{}", response.text().await.unwrap());
}
