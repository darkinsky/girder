// Temporary scratch: mirrors the module's doctest bodies against the by-path module.
#[path = "../src/api_key.rs"]
mod api_key;

#[test]
fn doctest_bodies_hold() {
    {
        use api_key::{KeyCheck, check};
        assert_eq!(check("sk-abcdefghijklmnop"), KeyCheck::Ok);
        match check("\"sk-abcdefghijklmnop\"") {
            KeyCheck::Suspect { problem, hint } => {
                assert!(problem.contains("quote"));
                assert!(!hint.is_empty());
            }
            KeyCheck::Ok => panic!("a quoted key must not pass"),
        }
    }
    {
        use api_key::{KeyCheck, check, normalize};
        assert_eq!(normalize("  sk-abcdefghijklmnop \n"), "sk-abcdefghijklmnop");
        assert_eq!(normalize("Bearer sk-abcdefghijklmnop"), "sk-abcdefghijklmnop");
        assert_eq!(normalize("\"sk-abcdefghijklmnop\""), "sk-abcdefghijklmnop");
        assert_eq!(normalize("sk-abcdefghijklmnop"), "sk-abcdefghijklmnop");
        assert_eq!(check(&normalize(" \"sk-abcdefghijklmnop\"\r\n")), KeyCheck::Ok);
    }
}
