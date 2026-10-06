-- Вход через внешних провайдеров (OAuth 2.0: Google, GitHub, Яндекс ID).

-- Одноразовый код, с которым браузер возвращается на фронтенд после входа через провайдера;
-- фронтенд меняет его на токены. Сами токены в URL не попадают.
ALTER TYPE email_token_purpose ADD VALUE 'oauth_login';

-- Какой аккаунт у провайдера привязан к какому пользователю. У пользователя их может быть
-- несколько (Google и GitHub одновременно).
CREATE TABLE oauth_accounts (
    provider         text        NOT NULL,   -- google, github, yandex
    provider_user_id text        NOT NULL,   -- id пользователя у провайдера (sub, id)
    user_id          uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    email            citext,                 -- email, который вернул провайдер (на момент привязки)
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, provider_user_id)
);

CREATE INDEX oauth_accounts_user_id_idx ON oauth_accounts (user_id);

-- Начатые входы: state защищает от подделки запроса (CSRF), code_verifier — PKCE.
-- Живут 10 минут, удаляются при возврате от провайдера или фоновой чисткой.
CREATE TABLE oauth_states (
    state_hash    text PRIMARY KEY,
    provider      text        NOT NULL,
    code_verifier text        NOT NULL,
    expires_at    timestamptz NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
