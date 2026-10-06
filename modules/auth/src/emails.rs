//! Тексты писем. Ссылки ведут на страницы фронтенда (`APP_BASE_URL`), которые берут `token`
//! из адреса и вызывают соответствующий эндпоинт API.

use shared::mail::Email;

pub fn verify_email(base_url: &str, to: &str, token: &str) -> Email {
    Email {
        to: to.to_string(),
        subject: "Подтвердите email — Nexus".to_string(),
        text: format!(
            "Здравствуйте!\n\n\
             Чтобы завершить регистрацию в Nexus, перейдите по ссылке:\n\
             {base_url}/auth/verify-email?token={token}\n\n\
             Ссылка действует 24 часа. Если вы не регистрировались, просто проигнорируйте письмо.\n"
        ),
    }
}

pub fn login_link(base_url: &str, to: &str, token: &str) -> Email {
    Email {
        to: to.to_string(),
        subject: "Вход в Nexus".to_string(),
        text: format!(
            "Здравствуйте!\n\n\
             Чтобы войти в Nexus, перейдите по ссылке:\n\
             {base_url}/auth/email-login?token={token}\n\n\
             Если аккаунта с этим адресом ещё нет, он будет создан.\n\
             Ссылка действует 15 минут и сработает один раз. Если вы не запрашивали вход, \
             проигнорируйте письмо.\n"
        ),
    }
}

pub fn reset_password(base_url: &str, to: &str, token: &str) -> Email {
    Email {
        to: to.to_string(),
        subject: "Сброс пароля — Nexus".to_string(),
        text: format!(
            "Здравствуйте!\n\n\
             Чтобы задать новый пароль, перейдите по ссылке:\n\
             {base_url}/auth/reset-password?token={token}\n\n\
             Ссылка действует 1 час. После смены пароля все устройства будут разлогинены.\n\
             Если вы не запрашивали сброс, проигнорируйте письмо: пароль останется прежним.\n"
        ),
    }
}
