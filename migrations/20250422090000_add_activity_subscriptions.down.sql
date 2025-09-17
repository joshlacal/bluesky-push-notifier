-- Drop activity subscription preference toggle and table
DROP INDEX IF EXISTS idx_activity_subscriptions_subject;
DROP INDEX IF EXISTS idx_activity_subscriptions_subscriber;
DROP TABLE IF EXISTS activity_subscriptions;
ALTER TABLE notification_preferences
DROP COLUMN IF EXISTS activity_subscriptions;
