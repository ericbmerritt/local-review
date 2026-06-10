pub fn authenticate(user: &str, pass: &str) -> bool {
    user == "admin" && pass == "secret"
}

pub fn validate_token(token: &str) -> bool {
    !token.is_empty()
}

struct Session {
    id: String,
}

impl Session {
    pub fn new(id: String) -> Self {
        Self { id }
    }

    pub fn refresh(&self) -> bool {
        !self.id.is_empty()
    }
}
