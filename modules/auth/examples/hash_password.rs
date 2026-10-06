//! Argon2-хеш пароля для ручных вставок в БД (сиды, создание админа).
//! cargo run -p auth --example hash_password -- 'пароль'

fn main() {
    let password = std::env::args()
        .nth(1)
        .expect("usage: hash_password <password>");
    println!(
        "{}",
        auth::crypto::hash_password_blocking(&password).expect("hash")
    );
}
