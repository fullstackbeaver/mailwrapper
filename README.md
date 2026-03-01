# mailWrapper

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

### Récupérer des emails depuis une date
```
GET /accounts/perso/emails/search?from=contact@example.com&since=2026-02-01&folder=INBOX
```

Les paramètres sont tous optionnels mais au moins un est requis. Tu peux les combiner ou les utiliser séparément :

```
#### Tous les emails d'une adresse depuis une date
?from=boss@company.com&since=2026-02-15

#### Tous les emails depuis une date (tous expéditeurs)
?since=2026-02-15

#### Tous les emails d'une adresse (sans limite de date)
?from=newsletter@example.com
```

La réponse inclut un champ `count` et `criteria`.

---

## IMAP IDLE

Si `[webhook]` est configuré dans `config.toml`, mailWrapper surveille INBOX en temps réel et envoie un `POST` au webhook Windmill à chaque nouvel email :

```json
{ "event": "new_email", "folder": "INBOX" }
```

## Démarrage

1. modifier le `.env` avec vos informations d'authentification

1. lancer docker-compose pour démarrer mailWrapper :

```bash
# Lancer
docker compose up -d
```
