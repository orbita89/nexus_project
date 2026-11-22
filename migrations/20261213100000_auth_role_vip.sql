-- Роль vip: обычный пользователь, которому админ открыл чтение закрытых книг
-- (GET /api/v1/catalog/books/{slug}/epub). Встаёт между user и author, чтобы
-- проверки role >= 'author' не изменились; чтение книг проверяется явно (vip, admin).
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'vip' AFTER 'user';
