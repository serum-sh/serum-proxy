# Kubernetes deployment templates

The manifests that run this proxy in a customer cluster. They ship with the
proxy release they deploy, so the image pin and the manifest shape always move
together.

| File | Contents | When it applies |
| --- | --- | --- |
| `core.yaml` | Namespace, ConfigMap, Deployment | Always |
| `rbac.yaml` | ServiceAccount, ClusterRole, ClusterRoleBinding | Only with the Kubernetes connector |

Both files are valid YAML, but neither is applied as-is: `%{name}` markers stand
in for the values that vary per deployment. The renderer substitutes them and
concatenates the files it needs, in the order above.

## Markers

| Marker | File | Value |
| --- | --- | --- |
| `%{namespace}` | both | Namespace to deploy into. Default `serum`. Also names the cluster-scoped RBAC objects, as `%{namespace}-proxy-read`. |
| `%{image}` | `core.yaml` | Full image reference, pinned to the release tag this template shipped with. |
| `%{service_account}` | `core.yaml` | `serum-proxy` when `rbac.yaml` is applied too, otherwise `default`. |
| `%{config_yaml}` | `core.yaml` | The proxy configuration, becoming `/app/config.yaml`. |

Rendering rules:

- Substitution is strict. An unresolved marker, or a value for a marker that
  does not appear, is a rendering error rather than a silent gap.
- `%{config_yaml}` sits inside a YAML block scalar. Every line of the config
  after the first must be indented to the marker's column (4 spaces).
- Nothing else in these files is templated. `${VAR}` inside the rendered config
  is read by the proxy at startup, not by the renderer.

## The token, and other credentials

The manifests hold no secret material. They expect a Secret named `serum-proxy`
in the same namespace, created separately, for example:

```
kubectl -n serum create secret generic serum-proxy --from-literal=token=<token>
```

Key `token` reaches the proxy as `SERUM_PROXY_TOKEN`, the credential it
authenticates the outbound connection with. Every other key of that Secret
becomes an environment variable of the same name, which is how a backend
credential written as `${ES_TOKEN}` in the config gets its value. A key whose
name is not a valid environment variable name is skipped by the kubelet, so
name them in upper snake case.

## Applying

```
kubectl apply -f core.yaml            # plus -f rbac.yaml when it applies
```

The Secret must exist first, or the pod waits in `CreateContainerConfigError`.

Teardown:

```
kubectl delete namespace serum
kubectl delete clusterrole,clusterrolebinding -l app.kubernetes.io/name=serum-proxy
```

## Notes for operators

- The proxy dials out and never accepts inbound connections, so there is no
  Service and no Ingress. It needs outbound TCP 443 to the serum gateway;
  clusters with a default-deny egress policy need an allowance for it. The
  connection does not honour `HTTPS_PROXY`.
- The ConfigMap is mounted with `subPath`, so an edit to it does not reach a
  running pod. After changing configuration:
  `kubectl -n serum rollout restart deployment/serum-proxy`.
- The pod runs as uid 10001 with a read-only root filesystem, no capabilities,
  and no privilege escalation.
- `GET /healthz` on port 8088 reports connection state and always answers 200,
  so the liveness probe restarts a wedged process without cycling the pod
  during a gateway outage. Port-forward it to see connection state.
- The RBAC grant is read-only and enumerated: `get` and `list` on pods and
  events. Secrets and ConfigMaps are not named, so they cannot be read through
  this ServiceAccount.
