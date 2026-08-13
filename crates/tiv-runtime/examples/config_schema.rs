fn main() -> Result<(), serde_json::Error> {
    print!("{}", tiv_runtime::config::config_schema_json()?);
    Ok(())
}
