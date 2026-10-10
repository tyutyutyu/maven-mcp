use super::*;
#[test]
fn parses_execution_positions_and_maven_options() {
    let m = parse_maven_command("cd '/tmp/a b'&&env FLAG=1 ./mvnw -plcore,api -amd -rf :api --activate-profiles=ci,fast --define 'test=Foo#works,Bar' -Dit.test=IT -Dflag -f 'child/pom.xml' -s settings.xml clean verify help:effective-pom|tee report").unwrap();
    assert_eq!(m.modules, ["core", "api"]);
    assert!(m.also_make_dependents);
    assert_eq!(m.resume_from.as_deref(), Some(":api"));
    assert_eq!(m.profiles, ["ci", "fast"]);
    assert_eq!(m.tests, ["Foo#works", "Bar", "IT"]);
    assert_eq!(m.properties["flag"], "true");
    assert_eq!(m.lifecycle_goals, ["clean", "verify"]);
    assert_eq!(m.plugin_goals, ["help:effective-pom"]);
    assert_eq!(m.pom_file.as_deref(), Some("child/pom.xml"));
    for command in [
        "echo mvn test",
        "printf '%s' 'mvn test'",
        "cat <<EOF\nmvn test\nEOF",
        "echo $(mvn test)",
        "mvn 'unfinished",
        "# mvn test",
    ] {
        assert!(parse_maven_command(command).is_none(), "{command}");
    }
    assert_eq!(
        parse_maven_command("echo 'mvn test';mvnd test||echo fail")
            .unwrap()
            .lifecycle_goals,
        ["test"]
    );
    assert_eq!(
        repository_category("echo jar tf x.jar"),
        CommandCategory::Shell
    );
    assert_eq!(repository_category("rg TODO src"), CommandCategory::Shell);
    assert_eq!(
        repository_category("jar tf x.jar|rg META-INF"),
        CommandCategory::ResourceInspection
    );
}
