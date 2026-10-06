-- auth: редактирование профиля. Описание — documents/modules/auth.md («Профиль»).

-- Ссылка подтверждения нового адреса при смене email (письмо на новый адрес).
ALTER TYPE email_token_purpose ADD VALUE 'change_email';

-- Когда username меняли в последний раз: менять можно не чаще раза в 30 дней.
-- NULL — не меняли (в том числе сгенерированный при входе через провайдера).
ALTER TABLE users ADD COLUMN username_changed_at timestamptz;

-- Привязка провайдера из профиля: чей вход начат. NULL — обычный вход через провайдера.
ALTER TABLE oauth_states ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
