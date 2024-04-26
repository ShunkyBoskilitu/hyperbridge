import assert from "assert";
import { RequestStatus } from "../../../types";
import { PostRequestEventLog } from "../../../types/abi-interfaces/EthereumHostAbi";
import { RequestService } from "../../../services/request.service";


/**
 * Handles the PostRequest event from Evm Hosts
 */
export async function handlePostRequestEvent(
  event: PostRequestEventLog,
): Promise<void> {
  assert(event.args, "No handlePostRequestEvent args");

  const {
    blockNumber,
    transactionHash,
    args,
  } = event;


  let {data, dest, fee, from, nonce, source, timeoutTimestamp, to} = args;

  // Compute the request commitment
  let request_commitment = RequestService.computeRequestCommitment(
    source,
    dest,
    BigInt(nonce.toString()),
    BigInt(timeoutTimestamp.toString()),
    from,
    to,
    data
  );


  // Create the request entity
  await RequestService.findOrCreate(
    request_commitment,
    data,
    dest,
    BigInt(fee.toString()),
    from,
    BigInt(nonce.toString()),
    source,
    RequestStatus.SOURCE,
    BigInt(timeoutTimestamp.toString()),
    to
  );

  // Create request meta data entity
  await RequestService.updateRequestStatus(
    request_commitment,
    RequestStatus.SOURCE,
    BigInt(blockNumber),
    transactionHash
  );
}
