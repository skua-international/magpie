{{- define "magpie.name" -}}
{{- .Chart.Name -}}
{{- end -}}

{{- define "magpie.fullname" -}}
{{- .Release.Name -}}
{{- end -}}

{{- define "magpie.labels" -}}
app.kubernetes.io/name: {{ include "magpie.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
This chart's own dedicated namespace -- every resource it creates lands
here regardless of whatever namespace `helm install`/`upgrade` itself was
invoked with, so a plain `helm install` can never accidentally dump
everything into `default` (or wherever the operator's kubectl context
happens to be pointed). See templates/namespace.yaml, which creates it.
*/}}
{{- define "magpie.namespace" -}}
{{- .Values.namespace -}}
{{- end -}}

{{- define "magpie.controllerNamespace" -}}
{{- .Values.controller.namespace | default (include "magpie.namespace" .) -}}
{{- end -}}

{{- define "magpie.serverApiNamespace" -}}
{{- .Values.serverApi.namespace | default (include "magpie.namespace" .) -}}
{{- end -}}

{{/*
Resolves an image reference, falling back from a per-service repository/tag
override to the chart-wide default. Called as:
  include "magpie.image" (dict "root" $ "override" .Values.syncDaemon.image)
*/}}
{{- define "magpie.image" -}}
{{- $repo := .override.repository | default .root.Values.image.repository -}}
{{- $tag := .override.tag | default .root.Values.image.tag | default .root.Chart.AppVersion -}}
{{- printf "%s:%s" $repo $tag -}}
{{- end -}}

{{/*
imagePullSecrets block for a pod spec, or nothing at all if none are
configured. Called as: {{- include "magpie.imagePullSecrets" . | nindent 6 }}
*/}}
{{- define "magpie.imagePullSecrets" -}}
{{- if .Values.imagePullSecrets }}
imagePullSecrets:
{{- range .Values.imagePullSecrets }}
  - name: {{ . }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
DATABASE_URL for the shared cluster Postgres, as a container env entry
list (PGPASSWORD sourced from a Secret, DATABASE_URL composed from it via
Kubernetes' $(VAR) dependent-env-var expansion -- so the password itself
never appears in a template-rendered value). Called as:
  include "magpie.postgresEnv" $
*/}}
{{- define "magpie.postgresEnv" -}}
{{- if .Values.postgres.enabled }}
- name: PGPASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ required "postgres.existingSecret is required when postgres.enabled is true" .Values.postgres.existingSecret }}
      key: POSTGRES_PASSWORD
- name: DATABASE_URL
  value: "postgres://{{ .Values.postgres.user }}:$(PGPASSWORD)@{{ include "magpie.fullname" . }}-postgres:{{ .Values.postgres.port }}/{{ .Values.postgres.database }}"
{{- else }}
- name: DATABASE_URL
  value: {{ required "postgres.externalUrl is required when postgres.enabled is false" .Values.postgres.externalUrl | quote }}
{{- end }}
{{- end -}}

{{/*
Discrete Postgres host, for env vars that need HOST/PORT/USER/PASSWORD
separately rather than one connection URL (see postgres.appUser/
appSecretName and postgres_bootstrap.rs). Called as:
  include "magpie.postgresHost" .
*/}}
{{- define "magpie.postgresHost" -}}
{{- if .Values.postgres.enabled -}}
{{- printf "%s-postgres" (include "magpie.fullname" .) -}}
{{- else -}}
{{- required "postgres.host is required when postgres.enabled is false" .Values.postgres.host -}}
{{- end -}}
{{- end -}}

{{/*
initContainers entry that blocks until Postgres actually accepts
connections, via pg_isready -- reuses postgres.image (already pulled for
the chart-managed StatefulSet, or a reasonable image to pull anyway for
an external one) rather than adding a new image dependency just for
this. Exists because of a real, confirmed-live startup race: services
using magpie.postgresEnv start immediately and try to resolve Postgres'
*headless* Service by DNS, which returns no records at all until at
least one backing pod is Ready -- not just scheduled -- so a service
starting before Postgres finishes its own startup crashes on DNS
resolution, not just connection refused. Kubernetes' own CrashLoopBackoff
does recover this eventually, but this avoids the churn (and the
confusing DNS-lookup-failure error, which doesn't obviously point at
"Postgres isn't ready yet" the way a connection-refused would) entirely.
Called as: {{- include "magpie.waitForPostgresInit" . | nindent 8 }}
*/}}
{{- define "magpie.waitForPostgresInit" -}}
- name: wait-for-postgres
  image: {{ .Values.postgres.image }}
  # The postgres image runs as root by default (its own entrypoint drops
  # to the "postgres" user itself, but overriding command/args the way
  # this initContainer does bypasses that entirely) -- every Deployment
  # this attaches to sets runAsNonRoot: true at the pod level, which
  # rejects a container with no explicit non-root UID of its own,
  # confirmed live ("container has runAsNonRoot and image will run as
  # root"). pg_isready needs no particular UID, so nobody (65534) is a
  # safe, image-independent choice rather than guessing this image's own
  # built-in postgres UID.
  securityContext:
    runAsUser: 65534
    runAsNonRoot: true
  command:
    - sh
    - -c
    - |
      until pg_isready -h {{ include "magpie.postgresHost" . }} -p {{ .Values.postgres.port }}; do
        echo "waiting for postgres..."
        sleep 2
      done
{{- end -}}

{{/*
JWKS URL -- defaults to this chart's own services/identity, since that's
the issuer this chart actually deploys; only overridden if jwt.jwksUrl is
explicitly set to point at something else.
*/}}
{{- define "magpie.jwksUrl" -}}
{{- if .Values.jwt.jwksUrl -}}
{{- .Values.jwt.jwksUrl -}}
{{- else -}}
{{- printf "http://%s-identity:%v/.well-known/jwks.json" (include "magpie.fullname" .) .Values.identity.service.port -}}
{{- end -}}
{{- end -}}

{{/*
JWKS/issuer/audience env entries, shared by services/registry and
services/server-api (which verify tokens) and services/identity (which
issues them) -- all three must agree, so they all pull from this one
helper.
*/}}
{{- define "magpie.jwtEnv" -}}
- name: JWKS_URL
  value: {{ include "magpie.jwksUrl" . | quote }}
- name: JWT_ISSUER
  value: {{ required "jwt.issuer is required" .Values.jwt.issuer | quote }}
- name: JWT_AUDIENCE
  value: {{ required "jwt.audience is required" .Values.jwt.audience | quote }}
{{- end -}}
