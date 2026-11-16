-- Candidate only. Apply through the backend migration workflow after backup.
-- Durable CE project IDs, owners and memberships remain in pinned Overleaf/Mongo.
BEGIN;
CREATE SCHEMA IF NOT EXISTS latex_gateway;
DO $$BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='cbgpu_latex') THEN
 CREATE ROLE cbgpu_latex NOLOGIN;
END IF;END$$;
REVOKE ALL ON SCHEMA latex_gateway FROM PUBLIC, anon, authenticated;
CREATE TABLE IF NOT EXISTS latex_gateway.state (
 kind text NOT NULL CHECK(kind IN ('challenges','sessions','tickets','uploads','incoming')),
 id text NOT NULL, data jsonb NOT NULL CHECK(jsonb_typeof(data)='object'),
 expires double precision NOT NULL CHECK(expires>0 AND expires<'Infinity'::float8),
 PRIMARY KEY(kind,id)
);
CREATE INDEX IF NOT EXISTS latex_gateway_expiry ON latex_gateway.state(expires);
CREATE INDEX IF NOT EXISTS latex_gateway_user ON latex_gateway.state((data->>'user_id'),kind);
ALTER TABLE latex_gateway.state ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON latex_gateway.state FROM PUBLIC,anon,authenticated;
GRANT USAGE ON SCHEMA latex_gateway TO service_role;
GRANT SELECT,INSERT,UPDATE,DELETE ON latex_gateway.state TO service_role;
GRANT USAGE ON SCHEMA latex_gateway TO cbgpu_latex;
GRANT SELECT,INSERT,UPDATE,DELETE ON latex_gateway.state TO cbgpu_latex;
DO $$BEGIN IF NOT EXISTS(SELECT FROM pg_policies WHERE schemaname='latex_gateway' AND tablename='state' AND policyname='trusted_latex_service') THEN
 CREATE POLICY trusted_latex_service ON latex_gateway.state TO cbgpu_latex USING(true) WITH CHECK(true);
END IF;END$$;
CREATE OR REPLACE FUNCTION latex_gateway.authorized(subject uuid, native_session uuid, account_v2 boolean, compute boolean DEFAULT false)
RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog AS $$
DECLARE allowed boolean; profile jsonb;
BEGIN
 SELECT to_jsonb(p) INTO profile FROM auth.sessions s JOIN auth.users u ON u.id=s.user_id
 JOIN public.profiles p ON p.id=u.id WHERE s.id=native_session AND u.id=subject
 AND (s.not_after IS NULL OR s.not_after>now()) AND u.deleted_at IS NULL
 AND (u.banned_until IS NULL OR u.banned_until<=now());
 IF profile IS NULL THEN RETURN false; END IF;
 IF NOT account_v2 THEN RETURN coalesce((profile->>'is_approved')::boolean,true); END IF;
 IF coalesce(profile->>'account_status','ACTIVE') NOT IN ('ACTIVE','PENDING_VERIFICATION','REVERIFICATION_REQUIRED') THEN RETURN false; END IF;
 -- Identity/project continuity is independent of school compute entitlement.
 IF NOT compute THEN RETURN true; END IF;
 BEGIN
 EXECUTE 'SELECT public.cbgpu_service_can_use_resource($1,$2,$3)' INTO allowed USING subject,'school_compute','cbsh';
 EXCEPTION WHEN undefined_function THEN RETURN false;
 END;
 RETURN coalesce(allowed,false);
END $$;
REVOKE ALL ON FUNCTION latex_gateway.authorized(uuid,uuid,boolean,boolean) FROM PUBLIC,anon,authenticated;
GRANT EXECUTE ON FUNCTION latex_gateway.authorized(uuid,uuid,boolean,boolean) TO service_role;
GRANT EXECUTE ON FUNCTION latex_gateway.authorized(uuid,uuid,boolean,boolean) TO cbgpu_latex;
COMMIT;
