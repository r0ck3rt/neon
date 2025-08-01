/*-------------------------------------------------------------------------
 *
 * neon_ddl_handler.c
 *	  Captures updates to roles/databases using ProcessUtility_hook and
 *        sends them to the control ProcessUtility_hook. The changes are sent
 *        via HTTP to the URL specified by the GUC neon.console_url when the
 *        transaction commits. Forwarding may be disabled temporarily by
 *        setting neon.forward_ddl to false.
 *
 *        Currently, the transaction may abort AFTER
 *        changes have already been forwarded, and that case is not handled.
 *        Subtransactions are handled using a stack of hash tables, which
 *        accumulate changes. On subtransaction commit, the top of the stack
 *        is merged with the table below it.
 *
 *    Support event triggers for {privileged_role_name}
 *
 * IDENTIFICATION
 *	 contrib/neon/neon_dll_handler.c
 *
 *-------------------------------------------------------------------------
 */

#include "postgres.h"

#include <curl/curl.h>
#include <unistd.h>

#include "access/xact.h"
#include "catalog/pg_authid.h"
#include "catalog/pg_proc.h"
#include "commands/defrem.h"
#include "commands/event_trigger.h"
#include "commands/user.h"
#include "fmgr.h"
#include "libpq/crypt.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "parser/parse_func.h"
#include "tcop/pquery.h"
#include "tcop/utility.h"
#include "utils/acl.h"
#include "utils/guc.h"
#include "utils/hsearch.h"
#include "utils/memutils.h"
#include "utils/jsonb.h"
#include <utils/lsyscache.h>
#include <utils/syscache.h>

#include "neon_ddl_handler.h"
#include "neon_utils.h"
#include "neon.h"

static ProcessUtility_hook_type PreviousProcessUtilityHook = NULL;
static fmgr_hook_type next_fmgr_hook = NULL;
static needs_fmgr_hook_type next_needs_fmgr_hook = NULL;
static bool neon_event_triggers = true;

static const char *jwt_token = NULL;

/* GUCs */
static char *ConsoleURL = NULL;
static bool ForwardDDL = true;
static bool RegressTestMode = false;

/*
 * CURL docs say that this buffer must exist until we call curl_easy_cleanup
 * (which we never do), so we make this a static
 */
static char CurlErrorBuf[CURL_ERROR_SIZE];

typedef enum
{
	Op_Set,						/* An upsert: Either a creation or an alter */
	Op_Delete,
} OpType;

typedef struct
{
	char		name[NAMEDATALEN];
	Oid			owner;
	char		old_name[NAMEDATALEN];
	OpType		type;
} DbEntry;

typedef struct
{
	char		name[NAMEDATALEN];
	char		old_name[NAMEDATALEN];
	const char *password;
	OpType		type;
} RoleEntry;

/*
 * We keep one of these for each subtransaction in a stack. When a subtransaction
 * commits, we merge the top of the stack into the table below it. It is allocated in the
 * subtransaction's context.
 */
typedef struct DdlHashTable
{
	struct DdlHashTable *prev_table;
	size_t		subtrans_level;
	HTAB	   *db_table;
	HTAB	   *role_table;
} DdlHashTable;

static DdlHashTable RootTable;
static DdlHashTable *CurrentDdlTable = &RootTable;
static int SubtransLevel; /* current nesting level of subtransactions */

static void
PushKeyValue(JsonbParseState **state, char *key, char *value)
{
	JsonbValue	k,
				v;

	k.type = jbvString;
	k.val.string.len = strlen(key);
	k.val.string.val = key;
	v.type = jbvString;
	v.val.string.len = strlen(value);
	v.val.string.val = value;
	pushJsonbValue(state, WJB_KEY, &k);
	pushJsonbValue(state, WJB_VALUE, &v);
}

static char *
ConstructDeltaMessage()
{
	JsonbParseState *state = NULL;

	pushJsonbValue(&state, WJB_BEGIN_OBJECT, NULL);
	if (RootTable.db_table)
	{
		JsonbValue	dbs;
		HASH_SEQ_STATUS status;
		DbEntry    *entry;

		dbs.type = jbvString;
		dbs.val.string.val = "dbs";
		dbs.val.string.len = strlen(dbs.val.string.val);
		pushJsonbValue(&state, WJB_KEY, &dbs);
		pushJsonbValue(&state, WJB_BEGIN_ARRAY, NULL);

		hash_seq_init(&status, RootTable.db_table);
		while ((entry = hash_seq_search(&status)) != NULL)
		{
			pushJsonbValue(&state, WJB_BEGIN_OBJECT, NULL);
			PushKeyValue(&state, "op", entry->type == Op_Set ? "set" : "del");
			PushKeyValue(&state, "name", entry->name);
			if (entry->owner != InvalidOid)
			{
				PushKeyValue(&state, "owner", GetUserNameFromId(entry->owner, false));
			}
			if (entry->old_name[0] != '\0')
			{
				PushKeyValue(&state, "old_name", entry->old_name);
			}
			pushJsonbValue(&state, WJB_END_OBJECT, NULL);
		}
		pushJsonbValue(&state, WJB_END_ARRAY, NULL);
	}

	if (RootTable.role_table)
	{
		JsonbValue	roles;
		HASH_SEQ_STATUS status;
		RoleEntry  *entry;

		roles.type = jbvString;
		roles.val.string.val = "roles";
		roles.val.string.len = strlen(roles.val.string.val);
		pushJsonbValue(&state, WJB_KEY, &roles);
		pushJsonbValue(&state, WJB_BEGIN_ARRAY, NULL);

		hash_seq_init(&status, RootTable.role_table);
		while ((entry = hash_seq_search(&status)) != NULL)
		{
			pushJsonbValue(&state, WJB_BEGIN_OBJECT, NULL);
			PushKeyValue(&state, "op", entry->type == Op_Set ? "set" : "del");
			PushKeyValue(&state, "name", entry->name);
			if (entry->password)
			{
#if PG_MAJORVERSION_NUM == 14
				char	   *logdetail;
#else
				const char *logdetail;
#endif
				char	   *encrypted_password;
				PushKeyValue(&state, "password", (char *) entry->password);
				encrypted_password = get_role_password(entry->name, &logdetail);

				if (encrypted_password)
				{
					PushKeyValue(&state, "encrypted_password", encrypted_password);
				}
				else
				{
					elog(ERROR, "Failed to get encrypted password: %s", logdetail);
				}
			}
			if (entry->old_name[0] != '\0')
			{
				PushKeyValue(&state, "old_name", entry->old_name);
			}
			pushJsonbValue(&state, WJB_END_OBJECT, NULL);
		}
		pushJsonbValue(&state, WJB_END_ARRAY, NULL);
	}
	{
		JsonbValue *result = pushJsonbValue(&state, WJB_END_OBJECT, NULL);
		Jsonb	   *jsonb = JsonbValueToJsonb(result);

		return JsonbToCString(NULL, &jsonb->root, 0 /* estimated_len */ );
	}
}

#define ERROR_SIZE 1024

typedef struct
{
	char		str[ERROR_SIZE];
	size_t		size;
} ErrorString;

static size_t
ErrorWriteCallback(char *ptr, size_t size, size_t nmemb, void *userdata)
{
	/* Docs say size is always 1 */
	ErrorString *str = userdata;

	size_t		to_write = nmemb;

	/* +1 for null terminator */
	if (str->size + nmemb + 1 >= ERROR_SIZE)
		to_write = ERROR_SIZE - str->size - 1;

	/* Ignore everyrthing past the first ERROR_SIZE bytes */
	if (to_write == 0)
		return nmemb;
	memcpy(str->str + str->size, ptr, to_write);
	str->size += to_write;
	str->str[str->size] = '\0';
	return nmemb;
}

static void
SendDeltasToControlPlane()
{
	static CURL		*handle = NULL;

	if (!RootTable.db_table && !RootTable.role_table)
		return;
	if (!ConsoleURL)
	{
		elog(LOG, "ConsoleURL not set, skipping forwarding");
		return;
	}
	if (!ForwardDDL)
		return;

	if (handle == NULL)
	{
		struct curl_slist *headers = NULL;

		headers = curl_slist_append(headers, "Content-Type: application/json");
		if (headers == NULL)
		{
			elog(ERROR, "Failed to set Content-Type header");
		}

		if (jwt_token)
		{
			char		auth_header[8192];

			snprintf(auth_header, sizeof(auth_header), "Authorization: Bearer %s", jwt_token);
			headers = curl_slist_append(headers, auth_header);
			if (headers == NULL)
			{
				elog(ERROR, "Failed to set Authorization header");
			}
		}

		handle = alloc_curl_handle();

		curl_easy_setopt(handle, CURLOPT_CUSTOMREQUEST, "PATCH");
		curl_easy_setopt(handle, CURLOPT_HTTPHEADER, headers);
		curl_easy_setopt(handle, CURLOPT_URL, ConsoleURL);
		curl_easy_setopt(handle, CURLOPT_ERRORBUFFER, CurlErrorBuf);
		curl_easy_setopt(handle, CURLOPT_TIMEOUT, 3L /* seconds */ );
		curl_easy_setopt(handle, CURLOPT_WRITEFUNCTION, ErrorWriteCallback);
	}

	{
		char	   *message = ConstructDeltaMessage();
		ErrorString str;
		const int	num_retries = 5;
		CURLcode	curl_status;
		long		response_code;

		str.size = 0;

		curl_easy_setopt(handle, CURLOPT_POSTFIELDS, message);
		curl_easy_setopt(handle, CURLOPT_WRITEDATA, &str);

		for (int i = 0; i < num_retries; i++)
		{
			if ((curl_status = curl_easy_perform(handle)) == 0)
				break;
			elog(LOG, "Curl request failed on attempt %d: %s", i, CurlErrorBuf);
			pg_usleep(1000 * 1000);
		}
		if (curl_status != CURLE_OK)
			elog(ERROR, "Failed to perform curl request: %s", CurlErrorBuf);

		if (curl_easy_getinfo(handle, CURLINFO_RESPONSE_CODE, &response_code) != CURLE_UNKNOWN_OPTION)
		{
			if (response_code != 200)
			{
				if (str.size != 0)
				{
					elog(ERROR,
						 "Received HTTP code %ld from control plane: %s",
						 response_code,
						 str.str);
				}
				else
				{
					elog(ERROR,
						 "Received HTTP code %ld from control plane",
						 response_code);
				}
			}
		}
	}
}

static void
InitCurrentDdlTableIfNeeded()
{
	/* Lazy construction of DllHashTable chain */
	if (SubtransLevel > CurrentDdlTable->subtrans_level)
	{
		DdlHashTable *new_table = MemoryContextAlloc(CurTransactionContext, sizeof(DdlHashTable));
		new_table->prev_table = CurrentDdlTable;
		new_table->subtrans_level = SubtransLevel;
		new_table->role_table = NULL;
		new_table->db_table = NULL;
		CurrentDdlTable = new_table;
	}
}

static void
InitDbTableIfNeeded()
{
	InitCurrentDdlTableIfNeeded();
	if (!CurrentDdlTable->db_table)
	{
		HASHCTL		db_ctl = {};

		db_ctl.keysize = NAMEDATALEN;
		db_ctl.entrysize = sizeof(DbEntry);
		db_ctl.hcxt = CurTransactionContext;
		CurrentDdlTable->db_table = hash_create(
												"Dbs Created",
												4,
												&db_ctl,
												HASH_ELEM | HASH_STRINGS | HASH_CONTEXT);
	}
}

static void
InitRoleTableIfNeeded()
{
	InitCurrentDdlTableIfNeeded();
	if (!CurrentDdlTable->role_table)
	{
		HASHCTL		role_ctl = {};

		role_ctl.keysize = NAMEDATALEN;
		role_ctl.entrysize = sizeof(RoleEntry);
		role_ctl.hcxt = CurTransactionContext;
		CurrentDdlTable->role_table = hash_create(
												  "Roles Created",
												  4,
												  &role_ctl,
												  HASH_ELEM | HASH_STRINGS | HASH_CONTEXT);
	}
}

static void
PushTable()
{
	SubtransLevel += 1;
}

static void
MergeTable()
{
	DdlHashTable *old_table;

	Assert(SubtransLevel >= CurrentDdlTable->subtrans_level);
	if (--SubtransLevel >= CurrentDdlTable->subtrans_level)
	{
		return;
	}

	old_table = CurrentDdlTable;
	CurrentDdlTable = old_table->prev_table;

	if (old_table->db_table)
	{
		DbEntry    *entry;
		HASH_SEQ_STATUS status;

		InitDbTableIfNeeded();

		hash_seq_init(&status, old_table->db_table);
		while ((entry = hash_seq_search(&status)) != NULL)
		{
			DbEntry    *to_write = hash_search(
											   CurrentDdlTable->db_table,
											   entry->name,
											   HASH_ENTER,
											   NULL);

			to_write->type = entry->type;
			if (entry->owner != InvalidOid)
				to_write->owner = entry->owner;
			strlcpy(to_write->old_name, entry->old_name, NAMEDATALEN);
			if (entry->old_name[0] != '\0')
			{
				bool		found_old = false;
				DbEntry    *old = hash_search(
											  CurrentDdlTable->db_table,
											  entry->old_name,
											  HASH_FIND,
											  &found_old);

				if (found_old)
				{
					if (old->old_name[0] != '\0')
						strlcpy(to_write->old_name, old->old_name, NAMEDATALEN);
					else
						strlcpy(to_write->old_name, entry->old_name, NAMEDATALEN);
					hash_search(
								CurrentDdlTable->db_table,
								entry->old_name,
								HASH_REMOVE,
								NULL);
				}
			}
		}
		hash_destroy(old_table->db_table);
	}

	if (old_table->role_table)
	{
		RoleEntry  *entry;
		HASH_SEQ_STATUS status;

		InitRoleTableIfNeeded();

		hash_seq_init(&status, old_table->role_table);
		while ((entry = hash_seq_search(&status)) != NULL)
		{
			RoleEntry * old;
			bool found_old = false;
			RoleEntry  *to_write = hash_search(
											   CurrentDdlTable->role_table,
											   entry->name,
											   HASH_ENTER,
											   NULL);

			to_write->type = entry->type;
			to_write->password = entry->password;
			strlcpy(to_write->old_name, entry->old_name, NAMEDATALEN);
			if (entry->old_name[0] == '\0')
				continue;

			old = hash_search(
							  CurrentDdlTable->role_table,
							  entry->old_name,
							  HASH_FIND,
							  &found_old);
			if (!found_old)
				continue;
			strlcpy(to_write->old_name, old->old_name, NAMEDATALEN);
			hash_search(CurrentDdlTable->role_table,
						entry->old_name,
						HASH_REMOVE,
						NULL);
		}
		hash_destroy(old_table->role_table);
	}
}

static void
PopTable()
{
	Assert(SubtransLevel >= CurrentDdlTable->subtrans_level);
	if (--SubtransLevel < CurrentDdlTable->subtrans_level)
	{
		/*
		 * Current table gets freed because it is allocated in aborted
		 * subtransaction's memory context.
		 */
		CurrentDdlTable = CurrentDdlTable->prev_table;
	}
}

static void
NeonSubXactCallback(
					SubXactEvent event,
					SubTransactionId mySubid,
					SubTransactionId parentSubid,
					void *arg)
{
	switch (event)
	{
		case SUBXACT_EVENT_START_SUB:
			return PushTable();
		case SUBXACT_EVENT_COMMIT_SUB:
			return MergeTable();
		case SUBXACT_EVENT_ABORT_SUB:
			return PopTable();
		default:
			return;
	}
}

static void
NeonXactCallback(XactEvent event, void *arg)
{
	if (event == XACT_EVENT_PRE_COMMIT || event == XACT_EVENT_PARALLEL_PRE_COMMIT)
	{
		SendDeltasToControlPlane();
	}
	RootTable.role_table = NULL;
	RootTable.db_table = NULL;
	Assert(CurrentDdlTable == &RootTable);
}

static bool
IsPrivilegedRole(const char *role_name)
{
	Assert(role_name);

	return strcmp(role_name, privileged_role_name) == 0;
}

static void
HandleCreateDb(CreatedbStmt *stmt)
{
	DefElem    *downer = NULL;
	ListCell   *option;
	bool		found = false;
	DbEntry    *entry;

	InitDbTableIfNeeded();

	foreach(option, stmt->options)
	{
		DefElem    *defel = lfirst(option);

		if (strcmp(defel->defname, "owner") == 0)
			downer = defel;
	}

	entry = hash_search(CurrentDdlTable->db_table,
						stmt->dbname,
						HASH_ENTER,
						&found);
	if (!found)
		memset(entry->old_name, 0, sizeof(entry->old_name));

	entry->type = Op_Set;
	if (downer && downer->arg)
	{
		const char *owner_name = defGetString(downer);

		if (IsPrivilegedRole(owner_name))
			elog(ERROR, "could not create a database with owner %s", privileged_role_name);

		entry->owner = get_role_oid(owner_name, false);
	}
	else
	{
		entry->owner = GetUserId();
	}
}

static void
HandleAlterOwner(AlterOwnerStmt *stmt)
{
	const char *name;
	bool		found = false;
	DbEntry    *entry;
	const char *new_owner;

	if (stmt->objectType != OBJECT_DATABASE)
		return;
	InitDbTableIfNeeded();

	name = strVal(stmt->object);
	entry = hash_search(CurrentDdlTable->db_table,
						name,
						HASH_ENTER,
						&found);
	if (!found)
		memset(entry->old_name, 0, sizeof(entry->old_name));

	new_owner = get_rolespec_name(stmt->newowner);
	if (IsPrivilegedRole(new_owner))
		elog(ERROR, "could not alter owner to %s", privileged_role_name);

	entry->owner = get_role_oid(new_owner, false);
	entry->type = Op_Set;
}

static void
HandleDbRename(RenameStmt *stmt)
{
	bool		found = false;
	DbEntry    *entry;
	DbEntry    *entry_for_new_name;

	Assert(stmt->renameType == OBJECT_DATABASE);
	InitDbTableIfNeeded();
	entry = hash_search(CurrentDdlTable->db_table,
						stmt->subname,
						HASH_FIND,
						&found);

	entry_for_new_name = hash_search(CurrentDdlTable->db_table,
									 stmt->newname,
									 HASH_ENTER,
									 NULL);
	entry_for_new_name->type = Op_Set;

	if (found)
	{
		if (entry->old_name[0] != '\0')
			strlcpy(entry_for_new_name->old_name, entry->old_name, NAMEDATALEN);
		else
			strlcpy(entry_for_new_name->old_name, entry->name, NAMEDATALEN);
		entry_for_new_name->owner = entry->owner;
		hash_search(CurrentDdlTable->db_table,
					stmt->subname,
					HASH_REMOVE,
					NULL);
	}
	else
	{
		strlcpy(entry_for_new_name->old_name, stmt->subname, NAMEDATALEN);
		entry_for_new_name->owner = InvalidOid;
	}
}

static void
HandleDropDb(DropdbStmt *stmt)
{
	bool		found = false;
	DbEntry    *entry;

	InitDbTableIfNeeded();

	entry = hash_search(CurrentDdlTable->db_table,
						stmt->dbname,
						HASH_ENTER,
						&found);
	entry->type = Op_Delete;
	entry->owner = InvalidOid;
	if (!found)
		memset(entry->old_name, 0, sizeof(entry->old_name));
}

static void
HandleCreateRole(CreateRoleStmt *stmt)
{
	bool		found = false;
	RoleEntry  *entry;
	DefElem    *dpass;
	ListCell   *option;

	InitRoleTableIfNeeded();

	dpass = NULL;
	foreach(option, stmt->options)
	{
		DefElem    *defel = lfirst(option);

		if (strcmp(defel->defname, "password") == 0)
			dpass = defel;
	}

	entry = hash_search(CurrentDdlTable->role_table,
						stmt->role,
						HASH_ENTER,
						&found);
	if (!found)
		memset(entry->old_name, 0, sizeof(entry->old_name));
	if (dpass && dpass->arg)
		entry->password = MemoryContextStrdup(CurTransactionContext, strVal(dpass->arg));
	else
		entry->password = NULL;
	entry->type = Op_Set;
}

static void
HandleAlterRole(AlterRoleStmt *stmt)
{
	char	   *role_name;
	DefElem    *dpass;
	ListCell   *option;
	bool		found = false;
	RoleEntry  *entry;

	InitRoleTableIfNeeded();

	role_name = get_rolespec_name(stmt->role);
	if (IsPrivilegedRole(role_name) && !superuser())
		elog(ERROR, "could not ALTER %s", privileged_role_name);

	dpass = NULL;
	foreach(option, stmt->options)
	{
		DefElem    *defel = lfirst(option);

		if (strcmp(defel->defname, "password") == 0)
			dpass = defel;
	}

	/* We only care about updates to the password */
	if (!dpass)
	{
		pfree(role_name);
		return;
	}

	entry = hash_search(CurrentDdlTable->role_table,
						role_name,
						HASH_ENTER,
						&found);
	if (!found)
		memset(entry->old_name, 0, sizeof(entry->old_name));
	if (dpass->arg)
		entry->password = MemoryContextStrdup(CurTransactionContext, strVal(dpass->arg));
	else
		entry->password = NULL;
	entry->type = Op_Set;

	pfree(role_name);
}

static void
HandleRoleRename(RenameStmt *stmt)
{
	bool		found = false;
	RoleEntry  *entry;
	RoleEntry  *entry_for_new_name;

	Assert(stmt->renameType == OBJECT_ROLE);
	InitRoleTableIfNeeded();

	entry = hash_search(CurrentDdlTable->role_table,
						stmt->subname,
						HASH_FIND,
						&found);

	entry_for_new_name = hash_search(CurrentDdlTable->role_table,
									 stmt->newname,
									 HASH_ENTER,
									 NULL);

	entry_for_new_name->type = Op_Set;
	if (found)
	{
		if (entry->old_name[0] != '\0')
			strlcpy(entry_for_new_name->old_name, entry->old_name, NAMEDATALEN);
		else
			strlcpy(entry_for_new_name->old_name, entry->name, NAMEDATALEN);
		entry_for_new_name->password = entry->password;
		hash_search(
					CurrentDdlTable->role_table,
					entry->name,
					HASH_REMOVE,
					NULL);
	}
	else
	{
		strlcpy(entry_for_new_name->old_name, stmt->subname, NAMEDATALEN);
		entry_for_new_name->password = NULL;
	}
}

static void
HandleDropRole(DropRoleStmt *stmt)
{
	ListCell   *item;

	InitRoleTableIfNeeded();

	foreach(item, stmt->roles)
	{
		RoleSpec   *spec = lfirst(item);
		bool		found = false;
		RoleEntry  *entry = hash_search(
										CurrentDdlTable->role_table,
										spec->rolename,
										HASH_ENTER,
										&found);

		entry->type = Op_Delete;
		entry->password = NULL;
		if (!found)
			memset(entry->old_name, 0, sizeof(entry->old_name));
	}
}


static void
HandleRename(RenameStmt *stmt)
{
	if (stmt->renameType == OBJECT_DATABASE)
		return HandleDbRename(stmt);
	else if (stmt->renameType == OBJECT_ROLE)
		return HandleRoleRename(stmt);
}


/*
 * Support for Event Triggers.
 *
 * In vanilla only superuser can create Event Triggers.
 *
 * We allow it for {privileged_role_name} by temporary switching to superuser. But as
 * far as event trigger can fire in superuser context we should protect
 * superuser from execution of arbitrary user's code.
 *
 * The idea was taken from Supabase PR series starting at
 *   https://github.com/supabase/supautils/pull/98
 */

static bool
neon_needs_fmgr_hook(Oid functionId) {

	return (next_needs_fmgr_hook && (*next_needs_fmgr_hook) (functionId))
		|| get_func_rettype(functionId) == EVENT_TRIGGEROID;
}

static void
LookupFuncOwnerSecDef(Oid functionId, Oid *funcOwner, bool *is_secdef)
{
	Form_pg_proc procForm;
	HeapTuple proc_tup = SearchSysCache1(PROCOID, ObjectIdGetDatum(functionId));

	if (!HeapTupleIsValid(proc_tup))
		ereport(ERROR,
				(errmsg("cache lookup failed for function %u", functionId)));

	procForm = (Form_pg_proc) GETSTRUCT(proc_tup);

	*funcOwner = procForm->proowner;
	*is_secdef = procForm->prosecdef;

	ReleaseSysCache(proc_tup);
}


PG_FUNCTION_INFO_V1(noop);
Datum noop(__attribute__ ((unused)) PG_FUNCTION_ARGS) { PG_RETURN_VOID();}

static void
force_noop(FmgrInfo *finfo)
{
    finfo->fn_addr   = (PGFunction) noop;
    finfo->fn_oid    = InvalidOid;           /* not a known function OID anymore */
    finfo->fn_nargs  = 0;                    /* no arguments for noop */
    finfo->fn_strict = false;
    finfo->fn_retset = false;
    finfo->fn_stats  = 0;                    /* no stats collection */
    finfo->fn_extra  = NULL;                 /* clear out old context data */
    finfo->fn_mcxt   = CurrentMemoryContext;
    finfo->fn_expr   = NULL;                 /* no parse tree */
}


/*
 * Skip executing Event Triggers execution for superusers, because Event
 * Triggers are SECURITY DEFINER and user provided code could then attempt
 * privilege escalation.
 *
 * Also skip executing Event Triggers when GUC neon.event_triggers has been
 * set to false. This might be necessary to be able to connect again after a
 * LOGIN Event Trigger has been installed that would prevent connections as
 * {privileged_role_name}.
 */
static void
neon_fmgr_hook(FmgrHookEventType event, FmgrInfo *flinfo, Datum *private)
{
	/*
	 * It can be other needs_fmgr_hook which cause our hook to be invoked for
	 * non-trigger function, so recheck that is is trigger function.
	 */
	if (flinfo->fn_oid != InvalidOid &&
		get_func_rettype(flinfo->fn_oid) != EVENT_TRIGGEROID)
	{
		if (next_fmgr_hook)
			(*next_fmgr_hook) (event, flinfo, private);

		return;
	}

	/*
	 * The {privileged_role_name} role can use the GUC neon.event_triggers to disable
	 * firing Event Trigger.
	 *
	 *   SET neon.event_triggers TO false;
	 *
	 * This only applies to the {privileged_role_name} role though, and only allows
	 * skipping Event Triggers owned by {privileged_role_name}, which we check by
	 * proxy of the Event Trigger function being owned by {privileged_role_name}.
	 *
	 * A role that is created in role {privileged_role_name} should be allowed to also
	 * benefit from the neon_event_triggers GUC, and will be considered the
	 * same as the {privileged_role_name} role.
	 */
	if (event == FHET_START
		&& !neon_event_triggers
		&& is_privileged_role())
	{
		Oid weak_superuser_oid = get_role_oid(privileged_role_name, false);

		/* Find the Function Attributes (owner Oid, security definer) */
		const char *fun_owner_name = NULL;
		Oid fun_owner = InvalidOid;
		bool fun_is_secdef = false;

		LookupFuncOwnerSecDef(flinfo->fn_oid, &fun_owner, &fun_is_secdef);
		fun_owner_name = GetUserNameFromId(fun_owner, false);

		if (IsPrivilegedRole(fun_owner_name)
			|| has_privs_of_role(fun_owner, weak_superuser_oid))
		{
			elog(WARNING,
				 "Skipping Event Trigger: neon.event_triggers is false");

			/*
			 * we can't skip execution directly inside the fmgr_hook so instead we
			 * change the event trigger function to a noop function.
			 */
			force_noop(flinfo);
		}
	}

	/*
	 * Fire Event Trigger if both function owner and current user are
	 * superuser. Allow executing Event Trigger function that belongs to a
	 * superuser when connected as a non-superuser, even when the function is
	 * SECURITY DEFINER.
	 */
    else if (event == FHET_START
		/* still enable it to pass pg_regress tests */
		&& !RegressTestMode)
	{
		/*
		 * Get the current user oid as of before SECURITY DEFINER change of
		 * CurrentUserId, and that would be SessionUserId.
		 */
		Oid current_role_oid = GetSessionUserId();
		bool role_is_super = superuser_arg(current_role_oid);

		/* Find the Function Attributes (owner Oid, security definer) */
		Oid function_owner = InvalidOid;
		bool function_is_secdef = false;
		bool function_is_owned_by_super = false;

		LookupFuncOwnerSecDef(flinfo->fn_oid, &function_owner, &function_is_secdef);

		function_is_owned_by_super = superuser_arg(function_owner);

		/*
		 * Refuse to run functions that belongs to a non-superuser when the
		 * current user is a superuser.
		 *
		 * We could run a SECURITY DEFINER user-function here and be safe with
		 * privilege escalation risks, but superuser roles are only used for
		 * infrastructure maintenance operations, where we prefer to skip
		 * running user-defined code.
		 */
		if (role_is_super && !function_is_owned_by_super)
		{
			char *func_name = get_func_name(flinfo->fn_oid);

			ereport(WARNING,
					(errmsg("Skipping Event Trigger"),
					 errdetail("Event Trigger function \"%s\" "
							   "is owned by non-superuser role \"%s\", "
							   "and current_user \"%s\" is superuser",
							   func_name,
							   GetUserNameFromId(function_owner, false),
							   GetUserNameFromId(current_role_oid, false))));

			/*
			 * we can't skip execution directly inside the fmgr_hook so
			 * instead we change the event trigger function to a noop
			 * function.
			 */
			force_noop(flinfo);
		}

	}

	if (next_fmgr_hook)
		(*next_fmgr_hook) (event, flinfo, private);
}

static Oid prev_role_oid = 0;
static int prev_role_sec_context = 0;
static bool switched_to_superuser = false;

/*
 * Switch tp superuser if not yet superuser.
 * Returns false if already switched to superuser.
 */
static bool
switch_to_superuser(void)
{
    Oid superuser_oid;

	if (switched_to_superuser)
		return false;
	switched_to_superuser = true;

	superuser_oid = get_role_oid("cloud_admin", true /*missing_ok*/);
	if (superuser_oid == InvalidOid)
		superuser_oid = BOOTSTRAP_SUPERUSERID;

    GetUserIdAndSecContext(&prev_role_oid, &prev_role_sec_context);
    SetUserIdAndSecContext(superuser_oid, prev_role_sec_context |
                                              SECURITY_LOCAL_USERID_CHANGE |
                                              SECURITY_RESTRICTED_OPERATION);
	return true;
}

static void
switch_to_original_role(void)
{
    SetUserIdAndSecContext(prev_role_oid, prev_role_sec_context);
    switched_to_superuser = false;
}

/*
 * ALTER ROLE ... SUPERUSER;
 *
 * Used internally to give superuser to a non-privileged role to allow
 * ownership of superuser-only objects such as Event Trigger.
 *
 *   ALTER ROLE foo SUPERUSER;
 *   ALTER EVENT TRIGGER ... OWNED BY foo;
 *   ALTER ROLE foo NOSUPERUSER;
 *
 * Now the EVENT TRIGGER is owned by foo, who can DROP it without having to be
 * superuser again.
 */
static void
alter_role_super(const char* rolename, bool make_super)
{
	AlterRoleStmt *alter_stmt = makeNode(AlterRoleStmt);

	DefElem *defel_superuser =
#if PG_MAJORVERSION_NUM <= 14
		makeDefElem("superuser", (Node *) makeInteger(make_super), -1);
#else
		makeDefElem("superuser", (Node *) makeBoolean(make_super), -1);
#endif

	RoleSpec *rolespec   = makeNode(RoleSpec);
	rolespec->roletype   = ROLESPEC_CSTRING;
	rolespec->rolename   = pstrdup(rolename);
	rolespec->location   = -1;

	alter_stmt->role = rolespec;
	alter_stmt->options = list_make1(defel_superuser);

#if PG_MAJORVERSION_NUM < 15
	AlterRole(alter_stmt);
#else
	/* ParseState *pstate, AlterRoleStmt *stmt */
	AlterRole(NULL, alter_stmt);
#endif

	CommandCounterIncrement();
}


/*
 * Changes the OWNER of an Event Trigger.
 *
 * Event Triggers can only be owned by superusers, so this ALTER ROLE with
 * SUPERUSER and then removes the property.
 */
static void
alter_event_trigger_owner(const char *obj_name, Oid role_oid)
{
	char* role_name = GetUserNameFromId(role_oid, false);

	alter_role_super(role_name, true);

	AlterEventTriggerOwner(obj_name, role_oid);
	CommandCounterIncrement();

	alter_role_super(role_name, false);
}


/*
 * Neon processing of the CREATE EVENT TRIGGER requires special attention and
 * is worth having its own ProcessUtility_hook for that.
 */
static void
ProcessCreateEventTrigger(
				   PlannedStmt *pstmt,
				   const char *queryString,
				   bool readOnlyTree,
				   ProcessUtilityContext context,
				   ParamListInfo params,
				   QueryEnvironment *queryEnv,
				   DestReceiver *dest,
				   QueryCompletion *qc)
{
	Node	   *parseTree = pstmt->utilityStmt;
	bool		sudo = false;

	/* We double-check that after local variable declaration block */
	CreateEventTrigStmt *stmt = (CreateEventTrigStmt *) parseTree;

	/*
	 * We are going to change the current user privileges (sudo) and might
	 * need after execution cleanup. For that we want to capture the UserId
	 * before changing it for our sudo implementation.
	 */
	const Oid current_user_id = GetUserId();
	bool current_user_is_super = superuser_arg(current_user_id);

	if (nodeTag(parseTree) != T_CreateEventTrigStmt)
	{
		ereport(ERROR,
				errcode(ERRCODE_INTERNAL_ERROR),
				errmsg("ProcessCreateEventTrigger called for the wrong command"));
	}

	/*
	 * Allow {privileged_role_name} to create Event Trigger, while keeping the
	 * ownership of the object.
	 *
	 * For that we give superuser membership to the role for the execution of
	 * the command.
	 */
	if (IsTransactionState() && is_privileged_role())
	{
		/* Find the Event Trigger function Oid */
		Oid func_oid = LookupFuncName(stmt->funcname, 0, NULL, false);

		/* Find the Function Owner Oid */
		Oid func_owner = InvalidOid;
		bool is_secdef = false;
		bool function_is_owned_by_super = false;

		LookupFuncOwnerSecDef(func_oid, &func_owner, &is_secdef);

		function_is_owned_by_super = superuser_arg(func_owner);

		if(!current_user_is_super && function_is_owned_by_super)
		{
			ereport(ERROR,
					(errcode(ERRCODE_INSUFFICIENT_PRIVILEGE),
					 errmsg("Permission denied to execute "
							"a function owned by a superuser role"),
					 errdetail("current user \"%s\" is not a superuser "
							   "and Event Trigger function \"%s\" "
							   "is owned by a superuser",
							   GetUserNameFromId(current_user_id, false),
							   NameListToString(stmt->funcname))));
		}

		if(current_user_is_super && !function_is_owned_by_super)
		{
			ereport(ERROR,
					(errcode(ERRCODE_INSUFFICIENT_PRIVILEGE),
					 errmsg("Permission denied to execute "
							"a function owned by a non-superuser role"),
					 errdetail("current user \"%s\" is a superuser "
							   "and function \"%s\" is "
							   "owned by a non-superuser",
							   GetUserNameFromId(current_user_id, false),
							   NameListToString(stmt->funcname))));
		}

		sudo = switch_to_superuser();
	}

	PG_TRY();
	{
		if (PreviousProcessUtilityHook)
		{
			PreviousProcessUtilityHook(
				pstmt,
				queryString,
				readOnlyTree,
				context,
				params,
				queryEnv,
				dest,
				qc);
		}
		else
		{
			standard_ProcessUtility(
				pstmt,
				queryString,
				readOnlyTree,
				context,
				params,
				queryEnv,
				dest,
				qc);
		}

		/*
		 * Now that the Event Trigger has been installed via our sudo
		 * mechanism, if the original role was not a superuser then change
		 * the event trigger ownership back to the original role.
		 *
		 * That way [ ALTER | DROP ] EVENT TRIGGER commands just work.
		 */
		if (IsTransactionState() && is_privileged_role())
		{
			if (!current_user_is_super)
			{
				/*
				 * Change event trigger owner to the current role (making
				 * it a privileged role during the ALTER OWNER command).
				 */
				alter_event_trigger_owner(stmt->trigname, current_user_id);
			}
		}
	}
	PG_FINALLY();
	{
		if (sudo)
			switch_to_original_role();
	}
	PG_END_TRY();
}


/*
 * Neon hooks for DDLs (handling privileges, limiting features, etc).
 */
static void
NeonProcessUtility(
				   PlannedStmt *pstmt,
				   const char *queryString,
				   bool readOnlyTree,
				   ProcessUtilityContext context,
				   ParamListInfo params,
				   QueryEnvironment *queryEnv,
				   DestReceiver *dest,
				   QueryCompletion *qc)
{
	Node	   *parseTree = pstmt->utilityStmt;

	/*
	 * The process utility hook for CREATE EVENT TRIGGER is its own
	 * implementation and warrant being addressed separately from here.
	 */
	if (nodeTag(parseTree) == T_CreateEventTrigStmt)
	{
		ProcessCreateEventTrigger(
				pstmt,
				queryString,
				readOnlyTree,
				context,
				params,
				queryEnv,
				dest,
				qc);
		return;
	}

	/*
	 * Other commands that need Neon specific implementations are handled here:
	 */
	switch (nodeTag(parseTree))
	{
		case T_CreatedbStmt:
			HandleCreateDb(castNode(CreatedbStmt, parseTree));
			break;
		case T_AlterOwnerStmt:
			HandleAlterOwner(castNode(AlterOwnerStmt, parseTree));
			break;
		case T_RenameStmt:
			HandleRename(castNode(RenameStmt, parseTree));
			break;
		case T_DropdbStmt:
			HandleDropDb(castNode(DropdbStmt, parseTree));
			break;
		case T_CreateRoleStmt:
			HandleCreateRole(castNode(CreateRoleStmt, parseTree));
			break;
		case T_AlterRoleStmt:
			HandleAlterRole(castNode(AlterRoleStmt, parseTree));
			break;
		case T_DropRoleStmt:
			HandleDropRole(castNode(DropRoleStmt, parseTree));
			break;
		case T_CreateTableSpaceStmt:
			if (!RegressTestMode)
			{
				ereport(ERROR,
					(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
					errmsg("CREATE TABLESPACE is not supported on Neon")));
			}
   			break;
		default:
			break;
	}

	if (PreviousProcessUtilityHook)
	{
		PreviousProcessUtilityHook(
			pstmt,
			queryString,
			readOnlyTree,
			context,
			params,
			queryEnv,
			dest,
			qc);
	}
	else
	{
		standard_ProcessUtility(
			pstmt,
			queryString,
			readOnlyTree,
			context,
			params,
			queryEnv,
			dest,
			qc);
	}
}

/*
 * Only {privileged_role_name} is granted privilege to edit neon.event_triggers GUC.
 */
static void
neon_event_triggers_assign_hook(bool newval, void *extra)
{
	if (IsTransactionState() && !is_privileged_role())
	{
		ereport(ERROR,
				(errcode(ERRCODE_INSUFFICIENT_PRIVILEGE),
				 errmsg("permission denied to set neon.event_triggers"),
				 errdetail("Only \"%s\" is allowed to set the GUC", privileged_role_name)));
	}
}


void
InitDDLHandler()
{
	PreviousProcessUtilityHook = ProcessUtility_hook;
	ProcessUtility_hook = NeonProcessUtility;

    next_needs_fmgr_hook = needs_fmgr_hook;
	needs_fmgr_hook = neon_needs_fmgr_hook;

	next_fmgr_hook = fmgr_hook;
	fmgr_hook = neon_fmgr_hook;

	RegisterXactCallback(NeonXactCallback, NULL);
	RegisterSubXactCallback(NeonSubXactCallback, NULL);

	/*
	 * The GUC neon.event_triggers should provide the same effect as the
	 * Postgres GUC event_triggers, but the neon one is PGC_USERSET.
	 *
	 * This allows using the GUC in the connection string and work out of a
	 * LOGIN Event Trigger that would break database access, all without
	 * having to edit and reload the Postgres configuration file.
	 */
	DefineCustomBoolVariable(
							 "neon.event_triggers",
							 "Enable firing of event triggers",
							 NULL,
							 &neon_event_triggers,
							 true,
							 PGC_USERSET,
							 0,
							 NULL,
							 neon_event_triggers_assign_hook,
							 NULL);

	DefineCustomStringVariable(
							   "neon.console_url",
							   "URL of the Neon Console, which will be forwarded changes to dbs and roles",
							   NULL,
							   &ConsoleURL,
							   NULL,
							   PGC_POSTMASTER,
							   0,
							   NULL,
							   NULL,
							   NULL);

	DefineCustomBoolVariable(
							 "neon.forward_ddl",
							 "Controls whether to forward DDL to the control plane",
							 NULL,
							 &ForwardDDL,
							 true,
							 PGC_SUSET,
							 0,
							 NULL,
							 NULL,
							 NULL);

	DefineCustomBoolVariable(
							 "neon.regress_test_mode",
							 "Controls whether we are running in the regression test mode",
							 NULL,
							 &RegressTestMode,
							 false,
							 PGC_SUSET,
							 0,
							 NULL,
							 NULL,
							 NULL);

	jwt_token = getenv("NEON_CONTROL_PLANE_TOKEN");
	if (!jwt_token)
	{
		elog(LOG, "Missing NEON_CONTROL_PLANE_TOKEN environment variable, forwarding will not be authenticated");
	}

}
