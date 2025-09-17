-- Remove via notification preferences
ALTER TABLE notification_preferences
DROP COLUMN via_likes,
DROP COLUMN via_reposts;