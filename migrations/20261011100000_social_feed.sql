-- social: лента. Записи людей, на которых подписан пользователь, по интересам и популярные
-- темы — см. documents/modules/social.md («Лента»).

-- Публичные коллекции тех, на кого подписан, новые сверху (collections_user_id_idx не по дате).
CREATE INDEX collections_user_public_idx ON collections (user_id, created_at DESC) WHERE is_public;
-- Популярные темы: больше сообщений, затем недавняя активность.
CREATE INDEX forum_threads_popular_idx ON forum_threads (posts_count DESC, last_post_at DESC, id DESC);
