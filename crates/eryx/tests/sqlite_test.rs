//! Test SQLite3 support in eryx sandbox
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "embedded")]

use eryx::Sandbox;

#[tokio::test]
async fn test_sqlite3_import() {
    let sandbox = Sandbox::embedded()
        .build()
        .expect("Failed to create sandbox");

    let result = sandbox
        .execute("import sqlite3; print(sqlite3.sqlite_version)")
        .await;
    assert!(
        result.is_ok(),
        "Failed to import sqlite3: {:?}",
        result.err()
    );

    let output = result.unwrap();
    assert!(
        !output.stdout.is_empty(),
        "sqlite3.sqlite_version returned empty"
    );
    println!("SQLite version: {}", output.stdout.trim());
}

#[tokio::test]
async fn test_sqlite3_in_memory_crud() {
    let sandbox = Sandbox::embedded()
        .build()
        .expect("Failed to create sandbox");

    let code = r#"
import sqlite3

# Create in-memory database
conn = sqlite3.connect(':memory:')
cursor = conn.cursor()

# Create table
cursor.execute('CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)')

# Insert data
cursor.execute("INSERT INTO users (name, age) VALUES ('Alice', 30)")
cursor.execute("INSERT INTO users (name, age) VALUES ('Bob', 25)")

# Query
cursor.execute('SELECT name, age FROM users ORDER BY age')
rows = cursor.fetchall()

conn.close()
print(rows)
"#;

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "CRUD test failed: {:?}", result.err());

    let output = result.unwrap();
    assert!(
        output.stdout.contains("Alice"),
        "Output should contain Alice"
    );
    assert!(output.stdout.contains("Bob"), "Output should contain Bob");
    println!("CRUD test output: {}", output.stdout);
}
