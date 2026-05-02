-- Seed the games catalog with 10 common games.
INSERT OR IGNORE INTO games (slug, display_name, engine) VALUES
    ('stardew-valley',  'Stardew Valley',    'monogame'),
    ('factorio',        'Factorio',           NULL),
    ('hades',           'Hades',              'supergiant'),
    ('terraria',        'Terraria',           'xna'),
    ('minecraft-java',  'Minecraft (Java)',   'java'),
    ('the-witcher-3',   'The Witcher 3',      'redengine3'),
    ('dwarf-fortress',  'Dwarf Fortress',     NULL),
    ('rimworld',        'RimWorld',           'unity'),
    ('hollow-knight',   'Hollow Knight',      'unity'),
    ('undertale',       'Undertale',          'gamemaker');
