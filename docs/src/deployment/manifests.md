# Kubernetes Manifests

If you prefer plain manifests over Helm, you can deploy `mcp-federation`
with a Deployment, Service, and ConfigMap.

## ConfigMap

Mount your `federation.yaml` as a ConfigMap:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: mcp-federation-config
  namespace: mcp-system
data:
  federation.yaml: |
    federation:
      listen: "0.0.0.0:8080"
      endpoint: "/mcp"
    servers:
      - alias: prod-k8s
        url: http://mcp-k8s.mcp-system.svc:8080/mcp
        transport: http-post
```

## Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: mcp-federation
  namespace: mcp-system
spec:
  replicas: 1
  selector:
    matchLabels:
      app: mcp-federation
  template:
    metadata:
      labels:
        app: mcp-federation
    spec:
      containers:
        - name: mcp-federation
          image: ghcr.io/alexconrey/mcp-federation:latest
          args: ["--config", "/etc/mcp-federation/federation.yaml"]
          ports:
            - containerPort: 8080
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8080
          volumeMounts:
            - name: config
              mountPath: /etc/mcp-federation
      volumes:
        - name: config
          configMap:
            name: mcp-federation-config
```

## Service

```yaml
apiVersion: v1
kind: Service
metadata:
  name: mcp-federation
  namespace: mcp-system
spec:
  selector:
    app: mcp-federation
  ports:
    - port: 8080
      targetPort: 8080
```
