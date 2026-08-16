-- Todo lo que el panel no miraba: releases, logs de cliente, procedencia de
-- cada versión, salud de los blobs, Pro, legal y el regalo de agosto.
--
-- Va en su **tercera función** por lo mismo que `admin_metrics_screen` (0044):
-- `create or replace` obliga a reescribir el cuerpo entero, y el de
-- `admin_metrics()` son 706 líneas que nadie quiere copiar cada vez que se
-- añade una cifra. El panel llama a las tres y las junta.
--
-- Mismo modelo de seguridad: SECURITY DEFINER + puerta por uid de admin.
--
-- ## Qué añade, y por qué cada bloque
--
--   * `releases` — cuánta gente corre cada versión, quién se ha quedado atrás y
--     a qué ritmo sube una versión nueva. Es la cifra que decide si una release
--     se relanza o se espera, y hasta ahora había que sacarla a mano contra
--     `devices`. La serie por día **no** sale de `devices` (esa tabla sólo
--     guarda el último visto, no historia): sale de `client_logs`, así que
--     cuenta únicamente máquinas con telemetría encendida. Sirve para la forma
--     de la curva, no como censo.
--
--   * `logs` — `client_logs` estaba en el panel como un contador y 25 líneas
--     recientes. Ahora: nivel por versión, por objetivo, los mensajes que más
--     se repiten y quién los emite. Un fallo que sólo le pasa a una versión se
--     ve aquí antes de que alguien lo cuente.
--
--   * `provenance` — `save_versions.device_name` (migración 0041) dice de qué
--     máquina salió cada copia. Lo que contesta: cuántas partidas reciben
--     versiones de **más de un ordenador**, que es la única prueba de que el
--     sync entre máquinas se usa de verdad y no es un backup con pasos de más.
--
--   * `blobs` — `integrity` y `verified_at` (0040) y `compress_attempts`
--     (0038). El bucle de reintentos de compresión de agosto no se vio venir
--     porque nadie miraba los intentos; aquí sale como histograma.
--
--   * `pro` — `profiles.first_pro_at` (0043): cuántos han tenido Pro alguna
--     vez, cuántos siguen, y cuánto tardaron desde el alta. Con dos clientes de
--     pago la media no significa nada, pero la lista entera cabe en pantalla.
--
--   * `legal` — `terms_acceptances` (0045): qué versión aceptó cada uno y desde
--     dónde. Un registro de aceptación que no se puede consultar no es un
--     registro.
--
--   * `grants` — `ops.grant_1gb_20260813`: el +1 GB de agosto. Guarda el límite
--     que cada cuenta tenía **antes**, así que es lo único que permite ver si
--     la reversión se ha hecho o sigue pendiente.
--
-- Horas y días en Europe/Madrid, como las otras dos.

create or replace function public.admin_metrics_extra()
returns jsonb
language plpgsql
security definer
set search_path = public
as $$
declare
  admin_uid constant uuid := 'f08eb80b-cbe8-4997-b69a-f2b9a5d6630a';
  tz        constant text := 'Europe/Madrid';
  -- La versión que se considera "la de ahora": la más alta vista en un
  -- dispositivo activo, ordenada como número y no como texto (si no, 1.1.10
  -- iría antes que 1.1.9 y la lista de rezagados saldría al revés).
  newest    text;
  out_json  jsonb;
begin
  if auth.uid() is distinct from admin_uid then
    raise exception 'not authorized' using errcode = '42501';
  end if;

  select d.app_version into newest
    from devices d
   where d.app_version ~ '^[0-9]+\.[0-9]+\.[0-9]+$'
     and d.last_seen_at > now() - interval '30 days'
     -- 7.7.x es el árbol de desarrollo del propio admin, no una release.
     and split_part(d.app_version, '.', 1)::int < 7
   order by string_to_array(d.app_version, '.')::int[] desc
   limit 1;

  with
  -- ------------------------------------------------------------- releases --
  ver as (
    select coalesce(d.app_version, '(?)') as v,
           count(*)                       as devices,
           count(distinct d.user_id)      as users,
           max(d.last_seen_at)            as last_seen,
           min(d.created_at)              as first_device
      from devices d
     where d.last_seen_at > now() - interval '30 days'
     group by 1
  ),
  ver_os as (
    select coalesce(d.app_version, '(?)') as v,
           coalesce(d.os, '(?)')          as os,
           count(*)                       as devices
      from devices d
     where d.last_seen_at > now() - interval '30 days'
     group by 1, 2
  ),
  -- Primera vez que cada versión dio señales de vida, para medir cuánto tarda
  -- en subir. `client_logs` es lo único con fecha *y* versión.
  ver_first as (
    select app_version as v, min(received_at) as first_log
      from client_logs
     where app_version is not null
     group by 1
  ),
  -- Serie de adopción: máquinas distintas por versión y día. Sólo cuenta las
  -- que mandan logs — ver la cabecera.
  ver_day as (
    select (received_at at time zone tz)::date as day,
           app_version                          as v,
           count(distinct device_fingerprint)   as machines
      from client_logs
     where received_at > now() - interval '30 days'
       and app_version is not null
     group by 1, 2
  ),
  laggards as (
    select p.email, d.device_name, d.os, d.app_version, d.last_seen_at
      from devices d
      join profiles p on p.user_id = d.user_id
     where d.last_seen_at > now() - interval '7 days'
       and d.app_version is distinct from newest
     order by d.last_seen_at desc
     limit 60
  ),
  -- ----------------------------------------------------------------- logs --
  lg as (
    select l.*, (l.received_at at time zone tz)::date as day
      from client_logs l
     where l.received_at > now() - interval '30 days'
  ),
  lg_level_version as (
    select coalesce(app_version, '(?)') as v, level, count(*) as c
      from lg where received_at > now() - interval '7 days'
     group by 1, 2
  ),
  lg_target as (
    select coalesce(target, '(?)') as target, level, count(*) as c
      from lg where received_at > now() - interval '7 days'
     group by 1, 2
     order by c desc
     limit 40
  ),
  lg_msg as (
    select message, level, coalesce(target, '(?)') as target,
           count(*) as c,
           count(distinct user_id) as users,
           count(distinct coalesce(app_version, '(?)')) as versions,
           max(received_at) as last_at
      from lg
     where level in ('error', 'warn', 'ERROR', 'WARN')
     group by 1, 2, 3
     order by c desc
     limit 40
  ),
  lg_day as (
    select day, level, count(*) as c
      from lg
     where received_at > now() - interval '14 days'
     group by 1, 2
  ),
  lg_users as (
    select p.email, count(*) as c,
           count(*) filter (where l.level in ('error', 'ERROR')) as errors,
           count(*) filter (where l.level in ('warn', 'WARN'))   as warns,
           max(l.app_version) as app_version
      from lg l join profiles p on p.user_id = l.user_id
     where l.received_at > now() - interval '7 days'
     group by 1
     order by c desc
     limit 25
  ),
  -- ----------------------------------------------------------- provenance --
  prov as (
    select v.device_name, count(*) as versions,
           count(distinct v.save_id) as saves,
           coalesce(sum(v.size_bytes), 0) as bytes,
           max(v.created_at) as last_at
      from save_versions v
     where v.device_name is not null and v.deleted_at is null
     group by 1
     order by versions desc
     limit 30
  ),
  prov_multi as (
    select s.game_slug, s.label, p.email,
           count(distinct v.device_name) as machines,
           count(*) as versions,
           max(v.created_at) as last_at
      from save_versions v
      join saves s    on s.id = v.save_id
      join profiles p on p.user_id = s.user_id
     where v.device_name is not null and v.deleted_at is null
     group by 1, 2, 3
    having count(distinct v.device_name) > 1
     order by machines desc, versions desc
     limit 40
  ),
  -- ---------------------------------------------------------------- blobs --
  blob_attempts as (
    select compress_attempts as attempts, count(*) as c,
           coalesce(sum(size_bytes), 0) as bytes
      from cloud_blobs
     group by 1
  ),
  blob_stuck as (
    select b.sha256, b.size_bytes, b.stored_bytes, b.encoding,
           b.compress_attempts, b.created_at, p.email
      from cloud_blobs b
      left join profiles p on p.user_id = b.user_id
     where b.compress_attempts >= 3
     order by b.compress_attempts desc, b.size_bytes desc
     limit 25
  ),
  -- ------------------------------------------------------------------ pro --
  pro as (
    select p.email, p.plan, p.first_pro_at, p.created_at,
           round(extract(epoch from p.first_pro_at - p.created_at) / 86400.0, 1) as days_to_pro,
           s.status, s.interval, s.renews_at, s.cancel_at
      from profiles p
      left join subscriptions s on s.user_id = p.user_id
     where p.first_pro_at is not null
     order by p.first_pro_at desc
  ),
  -- ---------------------------------------------------------------- legal --
  terms as (
    select t.version, t.source, coalesce(t.app_version, '(?)') as app_version,
           t.accepted_at, p.email
      from terms_acceptances t
      left join profiles p on p.user_id = t.user_id
     order by t.accepted_at desc
     limit 100
  ),
  -- --------------------------------------------------------------- grants --
  -- `storage_limit_bytes` en la tabla ops es el límite **anterior** al regalo:
  -- si el de `profiles` ya no coincide, esa cuenta aún lo tiene puesto.
  --
  -- Con `is distinct from` y no con `>`: antes del regalo el límite de casi
  -- todos era NULL (= el del plan, sin override), y `NULL > algo` no es falso,
  -- es NULL. Comparando a secas salían cero cuentas subidas **y** cero
  -- revertidas, que es justo la respuesta que no quieres de un contador que
  -- existe para vigilar una reversión con fecha.
  grants as (
    select g.user_id, g.plan, g.storage_limit_bytes as before_bytes,
           p.storage_limit_bytes as now_bytes, p.email, p.plan as plan_now,
           p.user_id is null as gone, g.saved_at
      from ops.grant_1gb_20260813 g
      left join profiles p on p.user_id = g.user_id
  )
  select jsonb_build_object(
    'generated_at', now(),
    'tz', tz,
    'schema_version', 1,

    'releases', jsonb_build_object(
      'newest', newest,
      'by_version', (select coalesce(jsonb_agg(row_to_json(x) order by x.devices desc), '[]'::jsonb)
                       from (select v.*, f.first_log from ver v
                               left join ver_first f on f.v = v.v) x),
      'by_version_os', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from ver_os x),
      'by_day', (select coalesce(jsonb_agg(row_to_json(x) order by x.day), '[]'::jsonb) from ver_day x),
      'laggards', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from laggards x),
      'on_newest', (select count(*) from devices
                     where app_version = newest and last_seen_at > now() - interval '30 days'),
      'active_30d', (select count(*) from devices where last_seen_at > now() - interval '30 days')
    ),

    'logs', jsonb_build_object(
      'total', (select count(*) from client_logs),
      'total_30d', (select count(*) from lg),
      'first_at', (select min(received_at) from client_logs),
      'by_level_30d', (select coalesce(jsonb_object_agg(level, c), '{}'::jsonb)
                         from (select level, count(*) c from lg group by 1) z),
      'by_level_version_7d', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from lg_level_version x),
      'by_target_7d', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from lg_target x),
      'top_messages_30d', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from lg_msg x),
      'by_day_14d', (select coalesce(jsonb_agg(row_to_json(x) order by x.day), '[]'::jsonb) from lg_day x),
      'top_users_7d', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from lg_users x),
      'recent_errors', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from (
          select l.received_at, l.level, l.target, l.message, l.fields,
                 l.app_version, l.device_os, l.device_name, p.email
            from client_logs l left join profiles p on p.user_id = l.user_id
           where l.level in ('error', 'warn', 'ERROR', 'WARN')
           order by l.received_at desc limit 60) x)
    ),

    'provenance', jsonb_build_object(
      'versions_total', (select count(*) from save_versions where deleted_at is null),
      'versions_named', (select count(*) from save_versions
                          where deleted_at is null and device_name is not null),
      'versions_cas', (select count(*) from save_versions
                        where deleted_at is null and content_addressed),
      'machines', (select count(distinct device_name) from save_versions where device_name is not null),
      'by_device', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from prov x),
      'multi_device_saves', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from prov_multi x),
      'multi_device_count', (select count(*) from prov_multi)
    ),

    'blobs', jsonb_build_object(
      'total', (select count(*) from cloud_blobs),
      'by_integrity', (select coalesce(jsonb_object_agg(coalesce(integrity, '(sin verificar)'), c), '{}'::jsonb)
                         from (select integrity, count(*) c from cloud_blobs group by 1) z),
      'verified', (select count(*) from cloud_blobs where verified_at is not null),
      'verified_bytes', (select coalesce(sum(size_bytes), 0) from cloud_blobs where verified_at is not null),
      'by_attempts', (select coalesce(jsonb_agg(row_to_json(x) order by x.attempts), '[]'::jsonb) from blob_attempts x),
      'stuck', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from blob_stuck x),
      'never_presigned', (select count(*) from cloud_blobs where last_presigned_at is null),
      'never_presigned_bytes', (select coalesce(sum(size_bytes), 0) from cloud_blobs where last_presigned_at is null),
      'purge_queued', (select count(*) from cloud_blobs where purge_after is not null)
    ),

    'pro', jsonb_build_object(
      'ever', (select count(*) from profiles where first_pro_at is not null),
      'now', (select count(*) from profiles where plan <> 'free'),
      'churned', (select count(*) from profiles where first_pro_at is not null and plan = 'free'),
      'median_days_to_pro', (select round(percentile_cont(0.5) within group (
                                order by extract(epoch from first_pro_at - created_at) / 86400.0)::numeric, 1)
                               from profiles where first_pro_at is not null),
      'people', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from pro x)
    ),

    'legal', jsonb_build_object(
      'total', (select count(*) from terms_acceptances),
      'users', (select count(distinct user_id) from terms_acceptances),
      'profiles', (select count(*) from profiles where deleted_at is null),
      'by_version', (select coalesce(jsonb_object_agg(version, c), '{}'::jsonb)
                       from (select version, count(*) c from terms_acceptances group by 1) z),
      'by_source', (select coalesce(jsonb_object_agg(coalesce(source, '(?)'), c), '{}'::jsonb)
                      from (select source, count(*) c from terms_acceptances group by 1) z),
      'recent', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from terms x)
    ),

    'grants', jsonb_build_object(
      'total', (select count(*) from grants),
      'still_raised', (select count(*) from grants
                        where not gone and now_bytes is distinct from before_bytes),
      'reverted', (select count(*) from grants
                    where not gone and now_bytes is not distinct from before_bytes),
      'gone', (select count(*) from grants where gone),
      'saved_at', (select max(saved_at) from grants),
      'by_plan_now', (select coalesce(jsonb_object_agg(coalesce(plan_now, '(baja)'), c), '{}'::jsonb)
                        from (select plan_now, count(*) c from grants
                               where not gone and now_bytes is distinct from before_bytes
                               group by 1) z),
      'people', (select coalesce(jsonb_agg(row_to_json(x)), '[]'::jsonb) from (
          select email, plan_now as plan, before_bytes, now_bytes from grants
           where not gone and now_bytes is distinct from before_bytes
           order by now_bytes desc nulls last limit 40) x)
    ),

    'budgets', jsonb_build_object(
      'max_versions', (select coalesce(jsonb_object_agg(coalesce(max_versions::text, 'sin tope'), c), '{}'::jsonb)
                         from (select max_versions, count(*) c from profiles group by 1) z),
      'max_manual_versions', (select coalesce(jsonb_object_agg(coalesce(max_manual_versions::text, 'sin tope'), c), '{}'::jsonb)
                                from (select max_manual_versions, count(*) c from profiles group by 1) z)
    )
  ) into out_json;

  return out_json;
end;
$$;

revoke all on function public.admin_metrics_extra() from public, anon;
grant execute on function public.admin_metrics_extra() to authenticated;
