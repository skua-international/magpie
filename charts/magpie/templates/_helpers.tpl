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

{{- define "magpie.controllerNamespace" -}}
{{- .Values.controller.namespace | default .Release.Namespace -}}
{{- end -}}

{{- define "magpie.serverApiNamespace" -}}
{{- .Values.serverApi.namespace | default .Release.Namespace -}}
{{- end -}}

{{/*
Resolves an image reference, falling back from a per-service repository/tag
override to the chart-wide default. Called as:
  include "magpie.image" (dict "root" $ "override" .Values.syncDaemon.image)
*/}}
{{- define "magpie.image" -}}
{{- $repo := .override.repository | default .root.Values.image.repository -}}
{{- $tag := .override.tag | default .root.Values.image.tag -}}
{{- printf "%s:%s" $repo $tag -}}
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
