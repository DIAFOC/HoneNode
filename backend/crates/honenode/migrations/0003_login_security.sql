CREATE TABLE login_challenges (
    token TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
CREATE TABLE login_limits (
    bucket TEXT PRIMARY KEY,
    window INTEGER NOT NULL,
    attempts INTEGER NOT NULL
);
