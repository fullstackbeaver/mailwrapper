# mailbridge

Wrapper HTTP léger (Rust/axum) pour IMAP/SMTP — conçu pour être appelé depuis Windmill.

## Endpoints

Tous les endpoints (sauf `/health`) nécessitent le header :
```
Authorization: Bearer your-secret-token-here
```

### Health
```
GET /health
```

### Lister les dossiers
```
GET /folders
```

### Lire les emails
```
GET /emails?folder=INBOX&limit=20
```

### Envoyer un email
```
POST /emails/send
{
  "to": "dest@example.com",
  "subject": "Sujet",
  "body": "Contenu",
  "html": false
}
```

### Déplacer un email
```
POST /emails/{uid}/move?folder=INBOX
{
  "folder": "Archives"
}
```

### Ajouter des labels/flags
```
POST /emails/{uid}/labels?folder=INBOX
{
  "labels": ["Seen", "Flagged"]
}
```

### Supprimer un email
```
DELETE /emails/{uid}?folder=INBOX
```

---

## IMAP IDLE

Si `[webhook]` est configuré dans `config.toml`, mailbridge surveille INBOX en temps réel et envoie un `POST` au webhook Windmill à chaque nouvel email :

```json
{ "event": "new_email", "folder": "INBOX" }
```

## Démarrage

```bash
# Copier et adapter la config
cp config.toml.example config.toml

# Lancer
docker compose up -d
```
