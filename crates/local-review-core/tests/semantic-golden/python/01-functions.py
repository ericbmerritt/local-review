def authenticate(user, password):
    return user == "admin" and password == "secret"


def validate_token(token):
    return bool(token)


class Session:
    def __init__(self, session_id):
        self.session_id = session_id

    def refresh(self):
        return bool(self.session_id)
