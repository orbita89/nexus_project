-- Тюнинг под слабый сервер (infra/postgres/postgresql.conf).
-- Статистика запросов: SELECT query, calls, mean_exec_time FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 20;
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

-- Горячие строки (счётчик комнаты, отметка прочтения) обновляются постоянно: запас места на
-- странице даёт HOT-апдейты без правки индексов, autovacuum приходит раньше.
ALTER TABLE chat_rooms SET (fillfactor = 90, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05);
ALTER TABLE chat_reads SET (fillfactor = 90, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05);
-- Сообщения только добавляются: чаще анализ, чтобы планировщик знал размер.
ALTER TABLE chat_messages SET (autovacuum_vacuum_insert_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05);
