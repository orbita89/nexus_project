// `sqlx::migrate!` встраивает миграции в бинарник на этапе компиляции.
// Без этого cargo не узнает, что новая миграция требует пересборки.
fn main() {
    println!("cargo:rerun-if-changed=../migrations");
}
