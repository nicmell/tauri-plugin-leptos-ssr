## Default Permission

Lets the plugin's pages send requests with a body over IPC and read streamed responses

#### This default permission set includes the following:

- `allow-fetch`
- `allow-fetch-read-body`
- `allow-fetch-cancel-body`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`leptos-ssr:allow-fetch`

</td>
<td>

Enables the fetch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`leptos-ssr:deny-fetch`

</td>
<td>

Denies the fetch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`leptos-ssr:allow-fetch-cancel-body`

</td>
<td>

Enables the fetch_cancel_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`leptos-ssr:deny-fetch-cancel-body`

</td>
<td>

Denies the fetch_cancel_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`leptos-ssr:allow-fetch-read-body`

</td>
<td>

Enables the fetch_read_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`leptos-ssr:deny-fetch-read-body`

</td>
<td>

Denies the fetch_read_body command without any pre-configured scope.

</td>
</tr>
</table>
